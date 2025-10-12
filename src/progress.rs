use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Progress {
    start: Instant,
    enqueued: Arc<AtomicI64>,
    checked: Arc<AtomicI64>,
    found: Arc<AtomicI64>,
    total_planned: Arc<AtomicI64>,
}

impl Progress {
    pub fn new(total_planned: i64) -> Self {
        Self {
            start: Instant::now(),
            enqueued: Arc::new(AtomicI64::new(0)),
            checked: Arc::new(AtomicI64::new(0)),
            found: Arc::new(AtomicI64::new(0)),
            total_planned: Arc::new(AtomicI64::new(total_planned.max(0))),
        }
    }
    pub fn inc_enqueued(&self) {
        self.enqueued.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_checked(&self) {
        self.checked.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_found(&self) {
        self.found.fetch_add(1, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> (i64, i64, i64, Duration) {
        (
            self.enqueued.load(Ordering::Relaxed),
            self.checked.load(Ordering::Relaxed),
            self.found.load(Ordering::Relaxed),
            self.start.elapsed(),
        )
    }
    pub fn total_planned(&self) -> i64 {
        self.total_planned.load(Ordering::Relaxed)
    }

    // Initialize counters from persisted state
    pub fn set_initial(&self, enqueued: i64, checked: i64, found: i64, total_planned: i64) {
        self.enqueued.store(enqueued, Ordering::Relaxed);
        self.checked.store(checked, Ordering::Relaxed);
        self.found.store(found, Ordering::Relaxed);
        self.total_planned.store(total_planned.max(0), Ordering::Relaxed);
    }
}