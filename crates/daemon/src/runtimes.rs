use std::{future::Future, sync::Arc};
use tokio::{
    runtime::{Builder, Runtime},
    sync::Semaphore,
    task::JoinHandle,
};

/// Construct outside an async runtime; shut down from a synchronous host boundary.
/// No engine or CPU job is spawned on the interactive executor by this type.
pub struct RuntimeSet {
    interactive: Runtime,
    work: Runtime,
    cpu: Runtime,
    cpu_slots: Arc<Semaphore>,
}
impl RuntimeSet {
    pub fn new(workers: usize, cpu_workers: usize) -> std::io::Result<Self> {
        if workers == 0 || cpu_workers == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "worker counts must be positive",
            ));
        }
        let interactive = Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .thread_name("excalibur-interactive")
            .enable_all()
            .build()?;
        let work = Builder::new_multi_thread()
            .worker_threads(workers)
            .max_blocking_threads(workers.clamp(4, 16))
            .on_thread_start(lower_background_priority)
            .thread_name("excalibur-work")
            .enable_all()
            .build()?;
        let cpu = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(cpu_workers)
            .thread_name("excalibur-cpu")
            .on_thread_start(lower_background_priority)
            .enable_all()
            .build()?;
        Ok(Self {
            interactive,
            work,
            cpu,
            cpu_slots: Arc::new(Semaphore::new(cpu_workers)),
        })
    }
    pub fn interactive(&self) -> tokio::runtime::Handle {
        self.interactive.handle().clone()
    }
    pub fn cpu_handle(&self) -> tokio::runtime::Handle {
        self.cpu.handle().clone()
    }
    pub fn work(&self) -> tokio::runtime::Handle {
        self.work.handle().clone()
    }
    pub fn spawn_interactive<F>(&self, f: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.interactive.spawn(f)
    }
    pub fn spawn_work<F>(&self, f: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.work.spawn(f)
    }
    /// Admission precedes spawn_blocking, bounding the blocking queue as well as threads.
    pub fn cpu<F, T>(&self, job: F) -> JoinHandle<Result<T, tokio::task::JoinError>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slots = self.cpu_slots.clone();
        self.cpu.spawn(async move {
            let _permit = slots
                .acquire_owned()
                .await
                .expect("CPU pool owns its semaphore");
            tokio::task::spawn_blocking(job).await
        })
    }
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.interactive.block_on(f)
    }
    pub fn shutdown(self, timeout: std::time::Duration) {
        self.work.shutdown_timeout(timeout);
        self.cpu.shutdown_timeout(timeout);
        self.interactive.shutdown_timeout(timeout);
    }
}
/// Best effort host priority. Failure does not claim a priority was applied.
fn lower_background_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: zero targets the calling thread; no pointers are passed.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: Darwin PRIO_DARWIN_THREAD/BG apply only to the calling thread.
    unsafe {
        libc::setpriority(3, 0, 0x1000);
    }
}
