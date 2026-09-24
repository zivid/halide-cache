//! In-memory request counters for /v1/stats. Everything here is lost on
//! restart by design: the numbers are operational, not billing.

use lager::Evicted;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Hit,
    Miss,
    Upload,
    Duplicate,
    Unauthorized,
}

#[derive(Default)]
pub struct Totals {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub uploads: AtomicU64,
    pub duplicates: AtomicU64,
    pub unauthorized: AtomicU64,
    pub evictions: AtomicU64,
    pub bytes_served: AtomicU64,
    pub bytes_received: AtomicU64,
    /// Bytes uploaded since the last eviction pass; drives opportunistic eviction.
    pub bytes_since_evict: AtomicU64,
}

#[derive(Default)]
pub struct Metrics {
    pub totals: Totals,
    /// (size, entries) from the latest eviction pass.
    last_scan: Mutex<(u64, usize)>,
}

impl Metrics {
    pub fn record(&self, event: Event, bytes: u64) {
        let t = &self.totals;
        match event {
            Event::Hit => {
                t.hits.fetch_add(1, Ordering::Relaxed);
                t.bytes_served.fetch_add(bytes, Ordering::Relaxed);
            }
            Event::Miss => {
                t.misses.fetch_add(1, Ordering::Relaxed);
            }
            Event::Upload | Event::Duplicate => {
                if matches!(event, Event::Upload) {
                    t.uploads.fetch_add(1, Ordering::Relaxed);
                } else {
                    t.duplicates.fetch_add(1, Ordering::Relaxed);
                }
                t.bytes_received.fetch_add(bytes, Ordering::Relaxed);
                t.bytes_since_evict.fetch_add(bytes, Ordering::Relaxed);
            }
            Event::Unauthorized => {
                t.unauthorized.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Records the outcome of an eviction pass.
    pub fn record_scan(&self, size: u64, entries: usize, evicted: &[Evicted]) {
        self.totals
            .evictions
            .fetch_add(evicted.len() as u64, Ordering::Relaxed);
        *self.last_scan.lock().unwrap() = (size, entries);
    }

    pub fn last_scan(&self) -> (u64, usize) {
        *self.last_scan.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_totals() {
        let m = Metrics::default();
        m.record(Event::Hit, 100);
        m.record(Event::Miss, 0);
        m.record(Event::Upload, 50);
        m.record(Event::Duplicate, 50);

        assert_eq!(m.totals.hits.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.misses.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.uploads.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.duplicates.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.bytes_served.load(Ordering::Relaxed), 100);
        assert_eq!(m.totals.bytes_received.load(Ordering::Relaxed), 100);
        assert_eq!(m.totals.bytes_since_evict.load(Ordering::Relaxed), 100);

        m.record_scan(10, 1, &[]);
        assert_eq!(m.last_scan(), (10, 1));
    }
}
