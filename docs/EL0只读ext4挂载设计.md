# EL0 只读 ext4 挂载设计

本文从 `FileSystem::mount` 开始，说明 ACT 用户态应用如何在 EL0 挂载 QEMU 的 virtio-blk 磁盘，并为后续模型文件读取提供对象。

## 1. 入口

ACT 应用在 `libos/src/apps/act_benchmark.rs` 中调用：

```rust
let filesystem = crate::fs::FileSystem::mount(info)?;
```

这里的 `info` 是 EL1 启动时写入、映射到 EL0 的只读 `UserBootInfo`。它只描述资源，不直接提供文件系统对象。

## 2. 挂载总流程

下面每一行都标出实际所在文件；`第三方`表示不属于本仓库的依赖源码。

```text
[libos/src/fs/ext4.rs] FileSystem::mount(info)
├── [libos/src/drivers/virtio_blk.rs] BlockDevices::discover(info)
│   ├── [abi/src/lib.rs] UserBootInfo.virtio_mmio
│   └── [libos/src/runtime/mod.rs] map_mmio(...)
│       ├── [abi/src/lib.rs] SYS_MAP_MMIO
│       └── [kernel/src/syscall/dispatch.rs] sys_map_mmio(...)
├── [libos/src/drivers/virtio_blk.rs] BlockDevices::next()
│   ├── 每 0x200 字节扫描一个 virtio transport
│   ├── [libos/src/drivers/virtio_blk.rs] read_volatile(magic/device_id)
│   ├── [第三方 virtio-drivers/src/transport/mmio.rs] MmioTransport::new
│   └── [第三方 virtio-drivers/src/device/blk.rs] VirtIOBlk::new
│       └── [libos/src/drivers/virtio_blk.rs] ExoVirtioHal::dma_alloc
├── [libos/src/fs/ext4.rs] BlockReader { device }
└── [第三方 ext4-view] Ext4::load(Box::new(reader))
    └── [libos/src/fs/ext4.rs] Ext4Read::read → BlockReader::read
```

挂载成功后返回：

```rust
FileSystem { inner: Ext4 }
```

`inner` 位于 EL0 的 libOS 内存中，Kernel 不保存 ext4 inode、路径、文件偏移或页缓存。

## 3. 枚举 Kernel 授权的设备

入口：[`libos/src/drivers/virtio_blk.rs`] 的 `BlockDevices::discover`。

`UserBootInfo.virtio_mmio` 是一个 `DeviceResource`：

| 字段 | 含义 |
|---|---|
| `base` | virtio-mmio 寄存器窗口的物理地址 PA |
| `size` | 整个连续 MMIO 窗口大小 |
| `intid` | 中断号；当前 virtio-blk 填 0，未使用 IRQ |

Kernel 在启动阶段从 DTB 找到窗口，并把窗口登记到当前 libOS 的 MMIO 授权表。EL0 只能使用 `UserBootInfo` 中已经给出的范围。

## 4. 校验并映射 MMIO

`discover` 检查：

1. `base` 非零。
2. `size` 至少能容纳一个 `0x200` 字节 transport。
3. `base` 和 `size` 都按页对齐。

然后调用 [`libos/src/runtime/mod.rs`] 的 `runtime::map_mmio`：

```text
EL0 runtime::map_mmio(pa, size, va)
└── SVC SYS_MAP_MMIO
    └── EL1 校验授权范围、页对齐和 VA 冲突
        └── 建立 Device-nGnRE 页表映射
```

当前固定的 EL0 虚拟地址是 `exo_abi::VIRTIO_MMIO_VA`，即 `0x4070_0000`。映射成功后，驱动的 `volatile` 访问会直接到达设备寄存器，不需要每次读写都进入 Kernel。

## 5. 扫描 virtio transport

[`libos/src/drivers/virtio_blk.rs`] 的 `BlockDevices::next` 每次检查一个 `0x200` 字节窗口：

1. 计算当前 transport 的 EL0 VA。
2. `read_volatile` 读取 VirtIO magic。
3. `read_volatile` 读取 device ID。
4. 不是 virtio-blk 就跳过。
5. 是块设备就创建 `MmioTransport`。

这样一个 MMIO 窗口可以包含多个 transport，迭代器会继续寻找下一个有效块设备。

## 6. 初始化 virtio-blk

`VirtIOBlk::new` 来自第三方 `virtio-drivers/src/device/blk.rs`，第一方适配发生在 [`libos/src/drivers/virtio_blk.rs`]：

```rust
VirtIOBlk::<ExoVirtioHal, _>::new(transport)
```

初始化过程包括：

1. 协商块设备支持的 virtio 特性。
2. 读取设备容量。
3. 创建块设备使用的 virtqueue。
4. 通过 `ExoVirtioHal` 为队列和请求缓冲区提供 DMA 内存。
5. 完成设备初始化。

## 7. BlockReader 适配层

第三方 `ext4-view` 需要的是任意字节读取接口，而第三方 `virtio-drivers/src/device/blk.rs` 只能按 512 字节扇区读取。因此第一方在 [`libos/src/fs/ext4.rs`] 定义 `BlockReader`：

```text
ext4_view::Ext4Read::read(start_byte, destination)
└── BlockReader::read
    ├── 非扇区对齐的开头：读一个临时扇区再截取
    ├── 中间完整区域：直接读多个完整扇区
    └── 非扇区对齐的结尾：读一个临时扇区再截取
```

它还会检查读取范围不能超过 virtio-blk 的容量。

## 8. Ext4::load

`Ext4::load` 来自第三方 `ext4-view`，不修改第三方代码。它通过 [`libos/src/fs/ext4.rs`] 中的 `Ext4Read` 实现回调读取磁盘内容，至少需要读取并校验 ext4 的超级块及相关元数据。

成功时，返回一个保存 ext4 解析状态的 `Ext4` 对象；失败时，`mount` 继续尝试下一个块设备。

因此这里的“挂载”不是 Kernel 挂载系统调用，而是：

```text
EL0 驱动对象 + BlockReader + ext4_view 解析状态
```

## 9. 挂载成功和失败

```text
找到有效 ext4
└── Ok(FileSystem { inner })

发现块设备但都不是 ext4
└── Err(FsError::NoExt4)

没有发现块设备
└── Err(FsError::NoBlockDevice)
```

ACT 应用再通过 `unwrap_or_else` 把错误转换成应用退出码 `0x530`。

## 10. 挂载后的文件读取

挂载本身不读取模型文件。后续 `FileSystem::read_mapped` 才会：

1. 用 `inner.open(path)` 查找文件。
2. 读取文件长度并检查最大值。
3. 通过 `Frame::allocate` 申请普通 Frame。
4. 通过 `Frame::map` 映射到指定的连续 EL0 VA。
5. 反复调用 `read_bytes`，最终回到 `BlockReader::read`。
6. 文件对象销毁时先解除映射，再释放 Frame。

文件内容的 CPU VA、Frame 的 PA 和 virtio DMA 地址不是同一个概念：普通文件 Frame 由 `Frame` 管理，virtio 请求缓冲区由 `ExoVirtioHal` 的 DMA 适配层管理。

## 11. 资源边界

| 层次 | 负责内容 |
|---|---|
| EL1 Kernel | DTB 发现、MMIO 授权与映射校验、DMA 连续页分配、页表和资源回收 |
| EL0 virtio 适配层 | 扫描寄存器、初始化 virtio-blk、提交扇区请求、处理 DMA bounce buffer |
| EL0 文件系统层 | `mount/open/read` 语义、ext4 对象、文件内容 Frame 映射 |
| 第三方库 | virtio 协议细节和 ext4 格式解析 |

当前 virtio-blk 不使用 `IRQ_BIND` 和 `Notification`；块请求由第三方驱动同步等待完成。后续若改成异步读，需要单独增加 virtio 中断授权、Notification 和 ACK 流程。
