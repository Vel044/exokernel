# EL0 只读 ext4 文件读取设计

本文从 ACT 性能基准读取模型文件的代码开始，说明文件如何从 ext4 磁盘进入 EL0 的普通 Frame，并最终作为模型输入切片使用。

## 1. 读取入口

入口位于 `libos/src/apps/act_benchmark.rs`：

```rust
let model = filesystem.read_mapped(
    "/model.safetensors",
    crate::drivers::act::MODEL_FILE_VA,
    crate::drivers::act::MODEL_FILE_MAX_SIZE,
)?;
```

三个参数分别表示：

| 参数 | 含义 |
|---|---|
| `path` | ext4 内的文件路径 |
| `user_va` | 文件内容映射到 EL0 的起始 VA |
| `maximum_size` | 允许读取的最大文件长度 |

模型当前映射到 `FRAME_ARENA_BASE`，最大允许 256 MiB；归一化参数紧接着映射到模型窗口之后的区域。

## 2. 调用链

```text
act_benchmark::run
└── FileSystem::read_mapped
    ├── Ext4::open(path)
    ├── file.metadata().len()
    ├── MappedFile::allocate(user_va, length)
    │   ├── Frame::allocate
    │   │   └── SVC SYS_FRAME_ALLOC → EL1 分配普通物理页
    │   └── Frame::map
    │       └── SVC SYS_FRAME_MAP → EL1 建立 EL0 VA→PA 映射
    ├── file.read_bytes(destination)
    │   └── BlockReader::read
    │       └── VirtIOBlk::read_blocks
    │           └── Virtqueue + ExoVirtioHal DMA
    └── 返回持有 Frame 和 Mapping 的 MappedFile
```

## 3. 打开文件和取得长度

`FileSystem::read_mapped` 先调用第三方 `ext4_view` 的 `open(path)`，通过 ext4 目录查找文件。

然后读取文件元数据长度，并检查：

1. 文件不能是空文件。
2. 文件不能超过调用者给出的上限。
3. 文件长度必须能被当前 EL0 的 `usize` 表示。

这一步只确定文件范围，还没有把文件内容读进内存。

## 4. 分配并映射文件 Frame

`MappedFile::allocate` 为文件内容建立连续的 EL0 VA，但不要求物理页连续：

```text
连续 EL0 VA
0x2000_0000 ─────── 文件内容 ─────── 0x...

物理 PA
Frame 0       Frame 1       Frame 2
可以不连续
```

分配过程：

1. 检查起始 VA 页对齐。
2. 计算文件需要的页数和结束 VA。
3. 确保范围位于 `FRAME_ARENA_BASE..FRAME_ARENA_END`。
4. 每批最多申请 `MAX_FRAME_PAGES` 个普通 Frame。
5. 使用 `Frame::map` 把每批 Frame 映射到连续的 EL0 VA。
6. 保存所有 Frame 和 Mapping 句柄。

普通文件 Frame 是 CPU 访问的 Normal 内存，不是 virtio 设备直接使用的 DMA 缓冲区。

## 5. Frame 系统调用边界

```text
EL0 Frame::allocate(pages, align)
└── runtime::frame_alloc
    └── SVC SYS_FRAME_ALLOC
        └── EL1 校验参数并返回不透明 FrameHandle
```

```text
EL0 Frame::map(frame, offset, pages, va, rights)
└── runtime::frame_map
    └── SVC SYS_FRAME_MAP
        └── EL1 校验 Frame 所有权、VA 范围和权限
            └── 建立页表映射并返回 MappingHandle
```

EL0 不直接得到或指定物理地址，只持有 Kernel 返回的句柄。文件内容通过映射后的 EL0 VA 访问。

## 6. 从 ext4 读取字节

Frame 映射完成后，`read_mapped` 循环调用 `read_bytes`，直到文件全部写入目标缓冲区。

第三方 ext4 解析器使用 `Ext4Read` trait 回调 `BlockReader::read`。`BlockReader` 负责把任意字节范围转换成 512 字节扇区请求：

```text
任意字节范围
├── 开头不对齐：读临时扇区并截取前缀
├── 中间完整扇区：直接批量读取
└── 结尾不对齐：读临时扇区并截取后缀
```

读取范围会先与设备容量比较，超出容量直接返回 I/O 错误。

## 7. virtio-blk 和 DMA

`BlockReader` 调用第三方 `VirtIOBlk::read_blocks`。第三方驱动通过 virtqueue 提交请求，第一方 `ExoVirtioHal` 提供底层内存：

1. `dma_alloc` 为 virtqueue 分配连续 DMA 页。
2. `share` 为普通文件缓冲区创建设备可见的 bounce buffer。
3. 无 IOMMU 时，设备使用物理 PA 作为 DMA address。
4. 设备完成读取后，`unshare` 把 bounce buffer 内容复制回文件 Frame。
5. `dmb osh` 保证设备和 CPU 之间的数据可见性。

这里必须区分三种地址：

| 地址 | 用途 |
|---|---|
| CPU VA | EL0 代码访问文件 Frame 或 bounce buffer |
| PA | Kernel 分配的实际物理页地址 |
| DMA address | 设备在无 IOMMU 时看到的地址，当前等于 PA |

## 8. 返回和生命周期

读取成功后返回 `MappedFile`。它同时持有：

```text
MappedFile
├── frames   → 普通物理 Frame 所有权
├── mappings → Frame 到 EL0 VA 的映射句柄
├── pointer  → 文件内容起始 CPU VA
└── length   → 有效文件字节数
```

模型通过 `model.as_slice()` 读取内容，因此 `MappedFile` 必须一直存活到 `Policy::from_files` 完成，甚至在策略对象仍借用模型切片时继续存活。

销毁顺序固定为：

```text
先 unmap Mapping
再 free Frame
```

这样不会留下指向已释放物理页的 EL0 页表映射。

## 9. 错误路径

| 阶段 | 错误 | ACT 应用退出码 |
|---|---|---|
| ext4 查找文件 | `InvalidFile` | `0x531` |
| 文件长度检查 | `TooLarge` | `0x531` |
| Frame 或 Mapping 申请 | `NoMemory` | `0x531` |
| 磁盘读取 | `Io` | `0x531` |

中途失败时，局部 `MappedFile` 会自动执行 `Drop`，解除已经建立的映射并释放已经申请的 Frame。

## 10. 层次边界

| 层次 | 负责内容 |
|---|---|
| ACT 应用策略 | 指定路径、目标 VA 和大小上限 |
| EL0 文件系统层 | 打开文件、申请 Frame、映射 VA、循环读取和资源回收 |
| EL0 virtio 适配层 | 扇区访问、virtqueue、DMA bounce 和缓存屏障 |
| EL1 Kernel | Frame/DMA 分配、所有权校验、页表映射和回收 |
| 第三方库 | ext4 解析和 virtio 协议实现 |

因此，`read_mapped` 不是把整个文件复制到统一 Heap，而是把文件内容装载到由 EL1 提供物理页、由 EL0 持有映射句柄的普通 Frame 区域。
