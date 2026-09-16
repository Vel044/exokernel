//! ACT四核行并行执行器。
//!
//! ACT运行时只知道“把若干输出行交给回调并等待完成”，本模块把这个抽象
//! 映射到外核Thread和共享内存原子屏障：调用线程负责最后一段，另外三个
//! 固定亲和性的worker各计算一段。模型算子仍在`act-runtime`中，本模块不
//! 解释Conv2d、Linear或Attention语义。
//!
//! worker属于同一libOS和同一VSpace，高频算子屏障无需每次通过Kernel
//! Notification。`generation + done bitmap`完全在EL0共享内存中同步，避免
//! 每个卷积产生多次SVC；IRQ等外部异步事件仍使用Kernel Notification。

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const WORKER_COUNT: usize = 3;
const PARALLEL_LANES: usize = WORKER_COUNT + 1;

static JOB_CONTEXT: AtomicU64 = AtomicU64::new(0);
static JOB_ROWS: AtomicUsize = AtomicUsize::new(0);
static JOB_KERNEL: AtomicUsize = AtomicUsize::new(0);
static JOB_GENERATION: AtomicU64 = AtomicU64::new(0);
static DONE_WORKERS: AtomicU64 = AtomicU64::new(0);

static JOB2_CONTEXT: AtomicU64 = AtomicU64::new(0);
static JOB2_COUNT: AtomicUsize = AtomicUsize::new(0);
static JOB2_KERNEL: AtomicUsize = AtomicUsize::new(0);
static JOB2_GENERATION: AtomicU64 = AtomicU64::new(0);
static DONE2_WORKERS: AtomicU64 = AtomicU64::new(0);

/// 持有三个固定CPU亲和性的worker线程。
///
/// worker在ACT应用生命周期内轮询共享generation。该设计让四核在连续算子
/// 间保持热状态，适合当前独占推理实验；以后若需要与其他任务节能共存，可在
/// 较长空闲期外加一次Notification，而不改变算子内部屏障。
pub(crate) struct ActParallelPool {
    _workers: [crate::thread::Thread; WORKER_COUNT],
}

impl ActParallelPool {
    /// 在指定三个CPU上建立worker；调用`predict`的线程自动成为第四个lane。
    pub(crate) fn create(
        worker_configs: [crate::thread::ThreadConfig; WORKER_COUNT],
    ) -> Result<Self, u64> {
        let workers = [
            crate::thread::Thread::spawn(act_worker, 0, worker_configs[0])?,
            crate::thread::Thread::spawn(act_worker, 1, worker_configs[1])?,
            crate::thread::Thread::spawn(act_worker, 2, worker_configs[2])?,
        ];
        // SAFETY:本池的对象活到前向传播结束；parallel_rows每次都等待三个
        // worker设置完成位，回调返回时不再有worker访问job context。
        unsafe {
            act_runtime::install_parallel_rows(Some(parallel_rows));
            act_runtime::install_parallel_jobs(Some(parallel_jobs));
        };
        Ok(Self { _workers: workers })
    }
}

extern "C" fn act_worker(worker: u64, _arg1: u64, _arg2: u64) -> ! {
    crate::runtime::configure_act_fpcr();
    let worker = worker as usize;
    let mut observed_generation = 0u64;
    let mut observed_job_generation = 0u64;
    loop {
        let generation = JOB_GENERATION.load(Ordering::Acquire);
        if generation != observed_generation {
            observed_generation = generation;
            let rows = JOB_ROWS.load(Ordering::Acquire);
            let context = JOB_CONTEXT.load(Ordering::Acquire) as *mut ();
            let kernel_address = JOB_KERNEL.load(Ordering::Acquire);
            // SAFETY:调度线程先发布有效函数地址和context，再Release发布新一代。
            let kernel: act_runtime::RowKernel = unsafe { core::mem::transmute(kernel_address) };
            let start_row = rows * worker / PARALLEL_LANES;
            let end_row = rows * (worker + 1) / PARALLEL_LANES;
            unsafe { kernel(context, start_row, end_row) };
            DONE_WORKERS.fetch_or(1u64 << worker, Ordering::Release);
            continue;
        }

        let job_generation = JOB2_GENERATION.load(Ordering::Acquire);
        if job_generation != observed_job_generation {
            observed_job_generation = job_generation;
            let jobs = JOB2_COUNT.load(Ordering::Acquire);
            let context = JOB2_CONTEXT.load(Ordering::Acquire) as *mut ();
            let kernel_address = JOB2_KERNEL.load(Ordering::Acquire);
            // SAFETY:job kernel使用C ABI，地址由同步dispatcher发布。
            let kernel: act_runtime::JobKernel = unsafe { core::mem::transmute(kernel_address) };
            let start = jobs * worker / PARALLEL_LANES;
            let end = jobs * (worker + 1) / PARALLEL_LANES;
            for job in start..end {
                unsafe { kernel(context, job) };
            }
            DONE2_WORKERS.fetch_or(1u64 << worker, Ordering::Release);
            continue;
        }
        core::hint::spin_loop();
    }
}

unsafe fn parallel_rows(context: *mut (), rows: usize, kernel: act_runtime::RowKernel) {
    JOB_CONTEXT.store(context as u64, Ordering::Relaxed);
    JOB_ROWS.store(rows, Ordering::Relaxed);
    JOB_KERNEL.store(kernel as usize, Ordering::Release);
    DONE_WORKERS.store(0, Ordering::Relaxed);
    // Release发布新generation，使三个worker随后Acquire到本轮context、rows和
    // 函数指针。单个前向传播串行提交job，不存在两个dispatcher并发覆盖。
    JOB_GENERATION.fetch_add(1, Ordering::Release);

    // 调用线程承担最后四分之一，避免它只等待worker而浪费一个物理核。
    kernel(context, rows * WORKER_COUNT / PARALLEL_LANES, rows);
    let expected = (1u64 << WORKER_COUNT) - 1;
    while DONE_WORKERS.load(Ordering::Acquire) != expected {
        core::hint::spin_loop();
    }
}

/// 把ACL scheduler的job列表映射到三个固定worker；CPU0承担最后一段。
unsafe extern "C" fn parallel_jobs(context: *mut (), jobs: usize, kernel: act_runtime::JobKernel) {
    JOB2_CONTEXT.store(context as u64, Ordering::Relaxed);
    JOB2_COUNT.store(jobs, Ordering::Relaxed);
    JOB2_KERNEL.store(kernel as usize, Ordering::Release);
    DONE2_WORKERS.store(0, Ordering::Relaxed);
    JOB2_GENERATION.fetch_add(1, Ordering::Release);

    let start = jobs * WORKER_COUNT / PARALLEL_LANES;
    for job in start..jobs {
        kernel(context, job);
    }
    let expected = (1u64 << WORKER_COUNT) - 1;
    while DONE2_WORKERS.load(Ordering::Acquire) != expected {
        core::hint::spin_loop();
    }
}
