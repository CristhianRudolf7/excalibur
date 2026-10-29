//! One process-wide worker, weak registrations, explicit low-activity windows.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct MaintenanceWindow {
    pub start_minute_utc: u16,
    pub end_minute_utc: u16,
}
impl MaintenanceWindow {
    pub fn contains(self, now: i64) -> bool {
        let minute = (now.rem_euclid(86400) / 60) as u16;
        if self.start_minute_utc <= self.end_minute_utc {
            minute >= self.start_minute_utc && minute < self.end_minute_utc
        } else {
            minute >= self.start_minute_utc || minute < self.end_minute_utc
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct MaintenancePolicy {
    pub enabled: bool,
    pub initial_delay: Duration,
    pub interval: Duration,
    pub window: Option<MaintenanceWindow>,
}
impl Default for MaintenancePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_delay: Duration::from_secs(300),
            interval: Duration::from_secs(86400),
            window: Some(MaintenanceWindow {
                start_minute_utc: 180,
                end_minute_utc: 240,
            }),
        }
    }
}
impl MaintenancePolicy {
    pub fn manual() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
    pub fn permits(self, elapsed: Duration, now: i64) -> bool {
        self.enabled
            && elapsed >= self.initial_delay
            && self.window.is_none_or(|window| window.contains(now))
    }
    pub(crate) fn validate(self) -> crate::Result<()> {
        if self.interval.is_zero()
            || self.window.is_some_and(|w| {
                w.start_minute_utc >= 1440
                    || w.end_minute_utc > 1440
                    || w.start_minute_utc == w.end_minute_utc
            })
        {
            return Err(crate::Error::Invalid("invalid maintenance policy"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct SchedulerStats {
    pub worker_threads: usize,
    pub registered: usize,
    pub runs: u64,
}
pub(crate) enum Outcome {
    Gone,
    Deferred,
    Complete,
}
type Work = dyn Fn(&tokio::runtime::Runtime, i64) -> Outcome + Send + Sync;
struct Entry {
    opened: Instant,
    next: Instant,
    policy: MaintenancePolicy,
    work: Arc<Work>,
}
struct Scheduler {
    entries: Mutex<BTreeMap<u64, Entry>>,
    wake: Condvar,
    next_id: AtomicU64,
    runs: AtomicU64,
}
static SHARED: OnceLock<Arc<Scheduler>> = OnceLock::new();
fn shared() -> &'static Arc<Scheduler> {
    SHARED.get_or_init(|| {
        let scheduler = Arc::new(Scheduler {
            entries: Mutex::new(BTreeMap::new()),
            wake: Condvar::new(),
            next_id: AtomicU64::new(1),
            runs: AtomicU64::new(0),
        });
        let worker = scheduler.clone();
        std::thread::Builder::new()
            .name("excalibur-storage-maintenance".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .expect("maintenance runtime");
                loop {
                    let entries = worker.entries.lock().unwrap();
                    let (entries, _) = worker
                        .wake
                        .wait_timeout(entries, Duration::from_secs(1))
                        .unwrap();
                    let now = Instant::now();
                    let utc = crate::unix_seconds();
                    let ready = entries
                        .iter()
                        .filter(|(_, e)| {
                            now >= e.next && e.policy.permits(now.duration_since(e.opened), utc)
                        })
                        .map(|(id, e)| (*id, e.work.clone(), e.policy.interval))
                        .collect::<Vec<_>>();
                    drop(entries);
                    for (id, work, interval) in ready {
                        let outcome = work(&runtime, utc);
                        let mut entries = worker.entries.lock().unwrap();
                        match outcome {
                            Outcome::Gone => {
                                entries.remove(&id);
                            }
                            Outcome::Deferred => {
                                if let Some(e) = entries.get_mut(&id) {
                                    e.next = Instant::now() + Duration::from_secs(60).min(interval);
                                }
                            }
                            Outcome::Complete => {
                                worker.runs.fetch_add(1, Ordering::Relaxed);
                                if let Some(e) = entries.get_mut(&id) {
                                    e.next = Instant::now() + interval;
                                }
                            }
                        }
                    }
                }
            })
            .expect("shared maintenance worker");
        scheduler
    })
}
pub(crate) fn register(policy: MaintenancePolicy, work: Arc<Work>) -> Option<u64> {
    if !policy.enabled {
        return None;
    }
    let scheduler = shared();
    let id = scheduler.next_id.fetch_add(1, Ordering::Relaxed);
    let opened = Instant::now();
    scheduler.entries.lock().unwrap().insert(
        id,
        Entry {
            opened,
            next: opened + policy.initial_delay,
            policy,
            work,
        },
    );
    scheduler.wake.notify_one();
    Some(id)
}
pub(crate) fn unregister(id: Option<u64>) {
    if let (Some(id), Some(scheduler)) = (id, SHARED.get()) {
        scheduler.entries.lock().unwrap().remove(&id);
        scheduler.wake.notify_one();
    }
}
pub fn maintenance_scheduler_stats() -> SchedulerStats {
    SHARED
        .get()
        .map_or_else(SchedulerStats::default, |s| SchedulerStats {
            worker_threads: 1,
            registered: s.entries.lock().unwrap().len(),
            runs: s.runs.load(Ordering::Relaxed),
        })
}
