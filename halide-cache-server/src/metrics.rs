//! In-memory request metrics for /v1/stats and /v1/history. Everything here is lost on restart by design: the numbers are
//! operational, not billing.

use lager::Evicted;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One-minute buckets, kept for 24 hours.
pub const BUCKET_SECONDS: u64 = 60;
const MAX_BUCKETS: usize = 24 * 60;

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Hit,
    Miss,
    Upload,
    Duplicate,
    Unauthorized,
}

#[derive(Default, Clone, serde::Serialize)]
pub struct Bucket {
    /// Unix time of the start of the minute.
    pub ts: u64,
    pub hits: u64,
    pub misses: u64,
    pub uploads: u64,
    pub bytes_served: u64,
    pub bytes_received: u64,
    /// Cache size after the last eviction pass at the end of this minute, if any.
    pub size_bytes: Option<u64>,
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
    history: Mutex<VecDeque<Bucket>>,
    /// (size, entries) from the latest eviction pass.
    last_scan: Mutex<(u64, usize)>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What one request adds to the counters. Computed once per event and applied
/// to the totals and the current history bucket alike.
#[derive(Default)]
struct Delta {
    hits: u64,
    misses: u64,
    uploads: u64,
    unauthorized: u64,
    bytes_served: u64,
    bytes_received: u64,
}

impl Delta {
    fn of(event: Event, bytes: u64) -> Self {
        let mut d = Delta::default();
        match event {
            Event::Hit => {
                d.hits = 1;
                d.bytes_served = bytes;
            }
            Event::Miss => d.misses = 1,
            Event::Upload | Event::Duplicate => {
                d.uploads = 1;
                d.bytes_received = bytes;
            }
            Event::Unauthorized => d.unauthorized = 1,
        }
        d
    }
}

impl Metrics {
    pub fn record(&self, event: Event, bytes: u64) {
        let d = Delta::of(event, bytes);
        let ts = now();

        let t = &self.totals;
        t.hits.fetch_add(d.hits, Ordering::Relaxed);
        t.misses.fetch_add(d.misses, Ordering::Relaxed);
        t.unauthorized.fetch_add(d.unauthorized, Ordering::Relaxed);
        t.bytes_served.fetch_add(d.bytes_served, Ordering::Relaxed);
        t.bytes_received
            .fetch_add(d.bytes_received, Ordering::Relaxed);
        t.bytes_since_evict
            .fetch_add(d.bytes_received, Ordering::Relaxed);
        // Uploads are the one place where the totals distinguish new from replaced.
        match event {
            Event::Upload => t.uploads.fetch_add(1, Ordering::Relaxed),
            Event::Duplicate => t.duplicates.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };

        {
            let mut history = self.history.lock().unwrap();
            let b = current_bucket(&mut history, ts);
            b.hits += d.hits;
            b.misses += d.misses;
            b.uploads += d.uploads;
            b.bytes_served += d.bytes_served;
            b.bytes_received += d.bytes_received;
        }
    }

    /// Records the outcome of an eviction pass.
    pub fn record_scan(&self, size: u64, entries: usize, evicted: &[Evicted]) {
        self.totals
            .evictions
            .fetch_add(evicted.len() as u64, Ordering::Relaxed);
        *self.last_scan.lock().unwrap() = (size, entries);
        let mut history = self.history.lock().unwrap();
        current_bucket(&mut history, now()).size_bytes = Some(size);
    }

    pub fn last_scan(&self) -> (u64, usize) {
        *self.last_scan.lock().unwrap()
    }

    /// Buckets covering the last `seconds`, oldest first.
    pub fn history(&self, seconds: u64) -> Vec<Bucket> {
        let since = now().saturating_sub(seconds);
        self.history
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.ts + BUCKET_SECONDS > since)
            .cloned()
            .collect()
    }
}

fn current_bucket(history: &mut VecDeque<Bucket>, ts: u64) -> &mut Bucket {
    let start = ts - ts % BUCKET_SECONDS;
    if history.back().is_none_or(|b| b.ts != start) {
        history.push_back(Bucket {
            ts: start,
            ..Default::default()
        });
        while history.len() > MAX_BUCKETS {
            history.pop_front();
        }
    }
    history.back_mut().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_totals_and_buckets() {
        let m = Metrics::default();
        m.record(Event::Hit, 100);
        m.record(Event::Miss, 0);
        m.record(Event::Upload, 50);
        m.record(Event::Duplicate, 50);

        assert_eq!(m.totals.hits.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.uploads.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.duplicates.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.bytes_received.load(Ordering::Relaxed), 100);

        let h = m.history(3600);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].hits, 1);
        assert_eq!(h[0].uploads, 2);
        assert_eq!(h[0].bytes_served, 100);
    }

    #[test]
    fn history_is_bounded() {
        let mut history = VecDeque::new();
        for i in 0..(MAX_BUCKETS as u64 + 10) {
            current_bucket(&mut history, i * BUCKET_SECONDS).hits += 1;
        }
        assert_eq!(history.len(), MAX_BUCKETS);
        assert_eq!(history.front().unwrap().ts, 10 * BUCKET_SECONDS);
    }
}
