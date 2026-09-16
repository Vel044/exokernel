# 1.Heap

## 当前设计

EL0 在启动时通过 `SYS_FRAME_ALLOC` 向 EL1 申请**固定大小 4 MiB** 普通 Frame，再通过`SYS_FRAME_MAP` 映射到固**定地址** `0x4080_0000`，最后由 `LockedHeap` 管理这段地址。Heap 本身**不会自动扩容**，Heap 用尽后就报错。

效果/用途：可以支持 EL0 中的小对象和动态数据结构，例如 `Vec`、`Box`、`String`、异步 Future、ext4/virtio/USB 驱动**对象**及临时缓冲区。

模型、图像、推理工作区等大块数据不放进 Heap，而是使用普通 **Frame（物理内存页框）**；设备直接访问的缓冲区使用 DMA 内存。

### exokernel 内如何使用 0x4080_0000 和 4MiB

这两个值是 EL0 虚拟地址空间的约定：`0x4080_0000` 是 Heap 起始虚拟地址，4MiB 是 Heap 的映射大小，因此 Heap 覆盖 `[0x4080_0000, 0x40c0_0000)`。它们不是固定的物理地址；物理页由 EL1 动态分配。

1. [`libos/src/main.rs`：`_start`](../libos/src/main.rs)：66行，计算 4MiB 对应的 1024 个 4KiB 页。
2. [`libos/src/kernel_api/frame.rs`：`Frame::allocate`](../libos/src/kernel_api/frame.rs)：通过 `SYS_FRAME_ALLOC` 请求 EL1 分配物理页。
3. [`libos/src/kernel_api/frame.rs`：`Frame::map`](../libos/src/kernel_api/frame.rs)：通过 `SYS_FRAME_MAP` 把物理页映射到 `0x4080_0000`。
4. [`libos/src/runtime/mod.rs`：`init_heap`](../libos/src/runtime/mod.rs)：让 `LockedHeap` 管理这段 EL0 虚拟地址。

之后 `Vec`、`Box`、`String` 等分配都在这段 4MiB 内完成，不会为每次小对象分配都进入 EL1。当前设计没有扩容流程，空间用尽时分配失败。

### 对比：SeL4 

seL4 Kernel 本身没有通用 Heap。
用户态可以自己准备一块Frame并配置分配器；没有配置 Heap 时，只有栈、静态内存和显式申请的 Frame 可以使用。

# 2.当前的UserBootInfo

定义位置：[`exokernel/abi/src/lib.rs`](../abi/src/lib.rs)，约在第 238 行。

```rust
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct UserBootInfo {
    pub magic: u32,
    pub version: u16,
    pub size: u16,
    
    pub uart: DeviceResource,
    pub xhci: DeviceResource,
    pub virtio_mmio: DeviceResource,
    pub xhci_transport: u32,
    pub cpu_count: u32,
    pub pci: PciHostInfo,
}6
```

`UserBootInfo` 是 EL1 传给 EL0 的启动信息表。EL1 填写设备和 CPU 资源，EL0 只读取它来决定可以使用哪些资源；`#[repr(C)]` 保证两边按照相同的内存布局解释这块数据。


# 3.Frame 物理内存页框

Frame 是一块可被 CPU 使用的物理 RAM，当前系统以 4 KiB 为一个基本页框。它本身没有虚拟地址，必须先通过页表映射到 EL0 的虚拟地址后，程序才能访问。

```text
Frame::allocate → 申请物理页
Frame::map      → 映射到 EL0 VA
程序            → 通过 VA 读写
Frame::unmap/free → 撤销映射并释放
```

Frame 主要用于模型、图像和推理工作区等大块内存。Heap 则是在一段已映射的 Frame 上运行的动态分配器，两者不是同一个概念。


# 4.Stack

## 4.1 当前 Exokernel

应用调用 `Thread::spawn` 后，通过 `SYS_THREAD_CREATE` 进入 EL1；Kernel 为线程分配独立的物理 Frame，映射成用户栈，并把 `SP_EL0` 设置在栈顶。当前每个新线程使用 256KiB 栈，退出时由 Kernel 回收。

```text
Thread::spawn
→ SYS_THREAD_CREATE
→ EL1 分配并映射 Stack Frame
→ 设置 SP_EL0
→ 运行线程
```

当前 Exokernel 的栈由 **Kernel 统一创建和管理**，应用不需要自己准备栈，因为要写裸寄存器。

## 4.2 seL4

seL4 Kernel 只负责创建 TCB 和保存寄存器，不自动分配用户栈。用户态通过下面几个调用自行完成：

1. `seL4_Untyped_Retype`：从 Untyped 创建 Stack Frame。
2. `seL4_ARM_Page_Map`：把 Stack Frame 映射进目标 VSpace。
3. `seL4_TCB_WriteRegisters`：把栈顶写入线程的初始 `SP`，同时设置入口地址。

`seL4_TCB_Configure` 主要负责把 TCB 连接到 CSpace、VSpace 和 IPC Buffer，不是专门的栈分配调用。

```text
seL4_Untyped_Retype
→ 创建 Stack Frame
seL4_ARM_Page_Map
→ 映射到目标 VSpace
seL4_TCB_WriteRegisters（这个比较重要 TCB提供些）
→ 设置线程初始 SP 和 PC
seL4_TCB_Resume
→ 启动线程
```
