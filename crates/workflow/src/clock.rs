//! Injected wall clock in UTC seconds. Tests use [`ManualClock`]; nothing sleeps.
use std::sync::atomic::{AtomicI64, Ordering};

pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}

#[derive(Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        excalibur_store::unix_seconds()
    }
}

pub struct ManualClock(AtomicI64);
impl ManualClock {
    pub fn new(at: i64) -> Self {
        Self(AtomicI64::new(at))
    }
    pub fn set(&self, at: i64) {
        self.0.store(at, Ordering::SeqCst);
    }
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
