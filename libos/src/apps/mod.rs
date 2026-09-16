//! 可执行的 libOS 实验和机器人应用。
//!
//! 本模块是EL0唯一场景分发边界。Cargo场景feature必须且只能选择一个；
//! `qemu-xhci`和`pi5-xhci`只表达硬件能力，不参与决定运行哪个应用。

#[cfg(all(
    not(feature = "ide"),
    not(any(
        feature = "app-system-smoke",
        feature = "app-process-smoke",
        feature = "app-uart-echo",
        feature = "app-usb-echo",
        feature = "app-uvc-smoke",
        feature = "app-scservo",
        feature = "app-act-inference",
        feature = "app-act-benchmark",
        feature = "app-robot-act-once",
        feature = "app-robot-observation",
        feature = "app-robot-action-replay"
    ))
))]
compile_error!("必须选择一个app-*场景；请通过LIBOS_APP调用构建脚本");

#[cfg(any(
    all(
        feature = "app-system-smoke",
        any(
            feature = "app-process-smoke",
            feature = "app-uart-echo",
            feature = "app-usb-echo",
            feature = "app-uvc-smoke",
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once"
        )
    ),
    all(
        feature = "app-process-smoke",
        any(
            feature = "app-uart-echo",
            feature = "app-usb-echo",
            feature = "app-uvc-smoke",
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once"
        )
    ),
    all(
        feature = "app-uart-echo",
        any(
            feature = "app-usb-echo",
            feature = "app-uvc-smoke",
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once"
        )
    ),
    all(
        feature = "app-usb-echo",
        any(
            feature = "app-uvc-smoke",
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once"
        )
    ),
    all(
        feature = "app-uvc-smoke",
        any(
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once"
        )
    ),
    all(feature = "app-scservo", feature = "app-act-inference"),
    all(feature = "app-scservo", feature = "app-robot-act-once"),
    all(feature = "app-act-inference", feature = "app-robot-act-once"),
    all(
        feature = "app-act-benchmark",
        any(
            feature = "app-system-smoke",
            feature = "app-process-smoke",
            feature = "app-uart-echo",
            feature = "app-usb-echo",
            feature = "app-uvc-smoke",
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once",
            feature = "app-robot-observation"
        )
    ),
    all(
        feature = "app-robot-observation",
        any(
            feature = "app-system-smoke",
            feature = "app-process-smoke",
            feature = "app-uart-echo",
            feature = "app-usb-echo",
            feature = "app-uvc-smoke",
            feature = "app-scservo",
            feature = "app-act-inference",
            feature = "app-robot-act-once"
        )
    )
))]
compile_error!("app-*场景互斥，一次只能编译一个");

#[cfg(all(feature = "qemu-xhci", feature = "pi5-xhci"))]
compile_error!("qemu-xhci与pi5-xhci平台能力互斥");

#[cfg(all(
    any(
        feature = "app-usb-echo",
        feature = "app-uvc-smoke",
        feature = "app-scservo",
        feature = "app-robot-act-once",
        feature = "app-robot-observation"
    ),
    not(any(feature = "qemu-xhci", feature = "pi5-xhci"))
))]
compile_error!("USB场景必须选择一个xHCI平台能力");

#[cfg(all(
    any(
        feature = "app-process-smoke",
        feature = "app-uart-echo",
        feature = "app-act-inference",
        feature = "app-act-benchmark"
    ),
    any(feature = "qemu-xhci", feature = "pi5-xhci")
))]
compile_error!("当前场景不使用xHCI，请移除xHCI平台feature");

#[cfg(any(feature = "ide", feature = "app-act-benchmark"))]
pub(crate) mod act_benchmark;
#[cfg(any(feature = "ide", feature = "app-act-inference"))]
pub(crate) mod act_inference;
#[cfg(any(feature = "ide", feature = "app-process-smoke"))]
pub(crate) mod process_smoke;
#[cfg(any(feature = "ide", feature = "app-robot-act-once"))]
pub(crate) mod robot_act_once;
#[cfg(any(feature = "ide", feature = "app-robot-observation"))]
pub(crate) mod robot_observation;
#[cfg(any(feature = "ide", feature = "app-robot-action-replay"))]
pub(crate) mod robot_action_replay;
#[cfg(any(feature = "ide", feature = "app-scservo"))]
pub(crate) mod scservo_app;
#[cfg(any(feature = "ide", feature = "app-system-smoke"))]
pub(crate) mod system_smoke;
#[cfg(any(
    feature = "ide",
    feature = "app-uart-echo",
    feature = "qemu-xhci",
    feature = "pi5-xhci"
))]
pub(crate) mod uart_echo;
#[cfg(all(
    any(feature = "qemu-xhci", feature = "pi5-xhci"),
    any(
        feature = "ide",
        feature = "app-usb-echo",
        feature = "app-system-smoke"
    )
))]
pub(crate) mod usb_echo;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod usb_task;
#[cfg(any(feature = "ide", feature = "app-uvc-smoke"))]
pub(crate) mod uvc_smoke;

/// 编译期选定的用户态场景。
///
/// `ide`会保留全部枚举成员，让rust-analyzer能够分析每条分发路径；正式构建
/// 只保留所选场景对应的成员，不把其他应用链接进最终ELF。
enum Application {
    #[cfg(any(feature = "ide", feature = "app-act-benchmark"))]
    ActBenchmark,
    #[cfg(any(feature = "ide", feature = "app-act-inference"))]
    ActInference,
    #[cfg(any(feature = "ide", feature = "app-process-smoke"))]
    ProcessSmoke,
    #[cfg(any(feature = "ide", feature = "app-uart-echo"))]
    UartEcho,
    #[cfg(any(feature = "ide", feature = "app-system-smoke"))]
    SystemSmoke,
    #[cfg(any(feature = "ide", feature = "app-usb-echo"))]
    UsbEcho,
    #[cfg(any(feature = "ide", feature = "app-uvc-smoke"))]
    UvcSmoke,
    #[cfg(any(feature = "ide", feature = "app-scservo"))]
    Scservo,
    #[cfg(any(feature = "ide", feature = "app-robot-act-once"))]
    RobotActOnce,
    #[cfg(any(feature = "ide", feature = "app-robot-observation"))]
    RobotObservation,
    #[cfg(any(feature = "ide", feature = "app-robot-action-replay"))]
    RobotActionReplay,
}

/// 从公共EL0启动流程进入唯一选定的应用场景。
pub(crate) fn run(info: &exo_abi::UserBootInfo) -> ! {
    dispatch(selected_application(), info)
}

/// 把互斥Cargo feature收敛成单个值；这里不执行任何设备初始化。
#[allow(unreachable_code)]
fn selected_application() -> Application {
    #[cfg(feature = "app-act-benchmark")]
    return Application::ActBenchmark;

    #[cfg(feature = "app-act-inference")]
    return Application::ActInference;

    #[cfg(feature = "app-robot-act-once")]
    return Application::RobotActOnce;

    #[cfg(feature = "app-robot-observation")]
    return Application::RobotObservation;
    #[cfg(feature = "app-robot-action-replay")]
    return Application::RobotActionReplay;

    #[cfg(feature = "app-process-smoke")]
    return Application::ProcessSmoke;

    #[cfg(feature = "app-uart-echo")]
    return Application::UartEcho;

    #[cfg(feature = "app-system-smoke")]
    return Application::SystemSmoke;

    #[cfg(feature = "app-usb-echo")]
    return Application::UsbEcho;

    #[cfg(feature = "app-uvc-smoke")]
    return Application::UvcSmoke;

    #[cfg(feature = "app-scservo")]
    return Application::Scservo;

    // rust-analyzer启用ide但不选择运行场景时，以综合测试作为类型检查入口。
    // 正式构建不会启用ide，因此不会执行这个fallback。
    #[cfg(feature = "ide")]
    return Application::SystemSmoke;

    // 缺少场景时前面的compile_error会给出构建期诊断；该表达式只用于让
    // 条件编译后的函数在类型系统中始终具有返回值。
    crate::runtime::exit(0x104)
}

/// 所有场景的统一运行时分发。
///
/// IDE模式下每个match分支都参与分析，因此调用目标不会变灰，Cmd+点击可以
/// 直接进入具体应用；正式构建时条件编译只留下一个有效分支。
fn dispatch(application: Application, info: &exo_abi::UserBootInfo) -> ! {
    match application {
        // 纯ACT性能基准：从只读ext4加载模型和一组冻结的真实观测，使用
        // CPU0..3执行一次100步ACT前向传播并记录各阶段耗时。它不初始化
        // PCI/xHCI，不访问摄像头或舵机，也绝不会产生物理运动。
        #[cfg(any(feature = "ide", feature = "app-act-benchmark"))]
        Application::ActBenchmark => act_benchmark::run(info),

        // ACT正确性实验：从只读ext4依次读取五组图片、六轴状态和PyTorch
        // 参考输出，比较每组完整的100x6个动作值。它验证推理数值，不测USB，
        // 不使能舵机扭矩。
        #[cfg(any(feature = "ide", feature = "app-act-inference"))]
        Application::ActInference => act_inference::run(info),

        // 单次真实机器人闭环：初始化xHCI并透传两台UVC和SCServo控制板，
        // 采集两张图、读取六轴位置、执行一次ACT推理，再以30Hz执行100步。
        // 该App会使能真实舵机扭矩并产生运动，正常或可处理错误结束时会关扭矩。
        #[cfg(any(feature = "ide", feature = "app-robot-act-once"))]
        Application::RobotActOnce => {
            prepare_xhci_logging(info);
            crate::runtime::puts(b"[libos] xHCI robot ACT one-shot start\r\n");
            usb_task::run_dedicated(info)
        }

        // 真实观测采集：通过xHCI读取handeye/fixed两台UVC的MJPEG，并读取
        // 六轴当前位置，写入可写ext4供宿主提取和后续基准复用。该App不运行
        // ACT、不写Goal Position，也不使能舵机扭矩。
        #[cfg(any(feature = "ide", feature = "app-robot-observation"))]
        Application::RobotObservation => {
            prepare_xhci_logging(info);
            crate::runtime::puts(b"[libos] xHCI robot observation capture start\r\n");
            usb_task::run_dedicated(info)
        }

        // 动作回放：不采集相机也不执行ACT，只从只读ext4加载已经生成的
        // 100x6个f32动作，通过CDC ACM和SCServo按30Hz发送给真实机械臂。
        // 这是会产生物理运动的诊断App，结束和错误路径都尝试关闭扭矩。
        #[cfg(any(feature = "ide", feature = "app-robot-action-replay"))]
        Application::RobotActionReplay => {
            prepare_xhci_logging(info);
            crate::runtime::puts(b"[libos] xHCI ACT action replay start\r\n");
            usb_task::run_dedicated(info)
        }

        // 多地址空间资源测试：创建两个VSpace，把不同Frame映射到相同EL0 VA，
        // 验证地址空间隔离、Mapping回收和VSPACE_DESTROY忙状态。它只使用
        // Frame/VSpace系统调用，不初始化任何设备。
        #[cfg(any(feature = "ide", feature = "app-process-smoke"))]
        Application::ProcessSmoke => process_smoke::run(),

        // PL011串口回显：映射Kernel授权的UART MMIO、创建Notification并绑定
        // UART IRQ，收到字符后直接写回PL011。该App用于Pi5串口交互，会永久
        // 等待中断，不涉及PCI、USB或舵机。
        #[cfg(any(feature = "ide", feature = "app-uart-echo"))]
        Application::UartEcho => uart_echo::run(info),

        // Kernel综合验收：依次测试Frame/VSpace、Endpoint IPC、Notification、
        // 静态优先级抢占和多核线程。带xHCI feature时还创建USB任务验证设备
        // 与计算线程并发；测试策略不会主动执行ACT运动轨迹。
        #[cfg(any(feature = "ide", feature = "app-system-smoke"))]
        Application::SystemSmoke => {
            #[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
            prepare_xhci_logging(info);
            system_smoke::run(info)
        }

        // USB串口回显：初始化PCI/xHCI和DMA，通过CrabUSB枚举CDC ACM或FTDI，
        // 持续执行Bulk IN后把收到的payload原样Bulk OUT。用于验证USB传输、
        // IRQ Notification和Event Ring，不解析SCServo，也不移动机械臂。
        #[cfg(any(feature = "ide", feature = "app-usb-echo"))]
        Application::UsbEcho => {
            prepare_xhci_logging(info);
            crate::runtime::puts(b"[libos] xHCI USB echo start\r\n");
            usb_task::run_dedicated(info)
        }

        // UVC单帧测试：初始化xHCI，打开第一台UVC摄像头，完成Probe/Commit和
        // 等时传输，重组一张MJPEG并写入可写ext4。它不解码ACT输入、不访问
        // SCServo，完成单帧验收后退出。
        #[cfg(any(feature = "ide", feature = "app-uvc-smoke"))]
        Application::UvcSmoke => {
            prepare_xhci_logging(info);
            crate::runtime::puts(b"[libos] xHCI UVC smoke start\r\n");
            usb_task::run_dedicated(info)
        }

        // SCServo协议测试：通过xHCI和CDC ACM/FTDI建立1Mbps Protocol 0总线，
        // PING并读取ID 1..6。只有额外启用app-scservo-move时才会平滑移动到
        // 校准中位；普通app-scservo构建是只读诊断。
        #[cfg(any(feature = "ide", feature = "app-scservo"))]
        Application::Scservo => {
            prepare_xhci_logging(info);
            crate::runtime::puts(b"[libos] xHCI SCServo start\r\n");
            usb_task::run_dedicated(info)
        }
    }
}

/// xHCI应用共用的EL0 UART接管步骤。
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
fn prepare_xhci_logging(info: &exo_abi::UserBootInfo) {
    if let Err(error) = uart_echo::smoke_test(info) {
        crate::runtime::puts(b"[libos] UART smoke failed=");
        crate::runtime::hex(error);
        crate::runtime::puts(b"\r\n");
        crate::runtime::exit(0x103);
    }
}
