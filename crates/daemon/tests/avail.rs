use excalibur_daemon::{Lane, LaneLimits, LaneScheduler, RuntimeSet};
use std::{
    sync::{Arc, Barrier},
    time::Duration,
};
#[tokio::test(start_paused = true)]
async fn master_has_reserved_concurrency_and_ten_percent_quota() {
    let s = LaneScheduler::new(LaneLimits {
        concurrency: 2,
        requests_per_minute: 10,
        tokens_per_minute: 100,
    })
    .unwrap();
    let background = s.acquire(Lane::Background, 90).await.unwrap();
    let other = s.clone();
    let queued = tokio::spawn(async move { other.acquire(Lane::User, 1).await });
    tokio::task::yield_now().await;
    let master = s.acquire(Lane::Master, 10).await.unwrap();
    assert!(!queued.is_finished());
    drop(master);
    drop(background);
    assert!(!queued.is_finished());
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(queued.await.unwrap().is_ok());
}
#[tokio::test]
async fn cancelled_waiters_release_priority_and_maintenance_yields_to_activity() {
    let s = LaneScheduler::new(LaneLimits::default()).unwrap();
    s.set_user_active(true);
    let other = s.clone();
    let job = tokio::spawn(async move { other.acquire(Lane::Maintenance, 1).await });
    tokio::task::yield_now().await;
    assert!(!job.is_finished());
    job.abort();
    let _ = job.await;
    assert_eq!(s.waiting(), [0; 4]);
    s.set_user_active(false);
    assert!(s.acquire(Lane::Maintenance, 1).await.is_ok());
}
#[test]
fn independent_runtimes_receive_under_eight_busy_workers_and_isolate_panics() {
    let runtimes = RuntimeSet::new(8, 2).unwrap();
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let barrier = barrier.clone();
        workers.push(runtimes.spawn_work(async move {
            barrier.wait();
            let until = std::time::Instant::now() + Duration::from_millis(250);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        }));
    }
    barrier.wait();
    let start = std::time::Instant::now();
    assert_eq!(
        runtimes
            .block_on(runtimes.spawn_interactive(async { 41 + 1 }))
            .unwrap(),
        42
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "interactive task starved by busy worker runtime"
    );
    let panic = runtimes.spawn_work(async { panic!("synthetic worker panic") });
    assert!(runtimes.block_on(panic).unwrap_err().is_panic());
    assert_eq!(
        runtimes.block_on(runtimes.spawn_work(async { 7 })).unwrap(),
        7
    );
    for worker in workers {
        runtimes.block_on(worker).unwrap();
    }
    runtimes.shutdown(Duration::from_secs(2));
}
#[test]
fn cpu_pool_is_bounded_and_reports_linux_priority() {
    let runtimes = RuntimeSet::new(1, 1).unwrap();
    let priority = runtimes.cpu(|| {
        #[cfg(target_os = "linux")]
        // SAFETY: reads calling thread priority, no pointers.
        unsafe {
            libc::getpriority(libc::PRIO_PROCESS, 0)
        }
        #[cfg(not(target_os = "linux"))]
        0
    });
    let priority = runtimes.block_on(priority).unwrap().unwrap();
    #[cfg(target_os = "linux")]
    assert!(priority >= 10);
    #[cfg(not(target_os = "linux"))]
    let _ = priority;
    let panic = runtimes.cpu(|| panic!("synthetic CPU panic"));
    assert!(runtimes.block_on(panic).unwrap().unwrap_err().is_panic());
    runtimes.shutdown(Duration::from_secs(2));
}
#[tokio::test(start_paused = true)]
async fn observed_provider_ceilings_reduce_admission_and_preserve_the_master_reserve() {
    let scheduler = LaneScheduler::new(LaneLimits {
        concurrency: 4,
        requests_per_minute: 100,
        tokens_per_minute: 1000,
    })
    .unwrap();
    scheduler.observe_provider_limits(Some((10, 100)));
    let work = scheduler.acquire(Lane::User, 90).await.unwrap();
    drop(work);
    let waiting = scheduler.clone();
    let queued = tokio::spawn(async move { waiting.acquire(Lane::Background, 1).await });
    tokio::task::yield_now().await;
    assert!(!queued.is_finished());
    assert!(scheduler.acquire(Lane::Master, 10).await.is_ok());
    queued.abort();
    let _ = queued.await;
    scheduler.observe_provider_limits(Some((0, u64::MAX))); // invalid observations cannot erase ceilings
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(scheduler.acquire(Lane::Master, 100).await.is_ok());
}
#[tokio::test(start_paused = true)]
async fn token_bucket_refills_incrementally_without_waiting_for_a_minute_boundary() {
    let scheduler = LaneScheduler::new(LaneLimits {
        concurrency: 2,
        requests_per_minute: 10,
        tokens_per_minute: 1000,
    })
    .unwrap();
    for _ in 0..10 {
        drop(scheduler.acquire(Lane::Master, 1).await.unwrap());
    }
    let other = scheduler.clone();
    let queued = tokio::spawn(async move { other.acquire(Lane::Master, 1).await });
    tokio::task::yield_now().await;
    assert!(!queued.is_finished());
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(queued.await.unwrap().is_ok());
}
