use lager::Evicted;
use serde::Serialize;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Hit,
    Miss,
    Upload,
    Duplicate,
}

#[derive(Default, Clone, Copy, Serialize)]
pub struct Counters {
    pub hits: u64,
    pub misses: u64,
    pub uploads: u64,
    pub duplicates: u64,
    pub bytes_served: u64,
    pub bytes_received: u64,
}

impl Counters {
    fn add(&mut self, event: Event, bytes: u64) {
        match event {
            Event::Hit => {
                self.hits += 1;
                self.bytes_served += bytes;
            }
            Event::Miss => self.misses += 1,
            Event::Upload => {
                self.uploads += 1;
                self.bytes_received += bytes;
            }
            Event::Duplicate => {
                self.duplicates += 1;
                self.bytes_received += bytes;
            }
        }
    }
}

#[derive(Default, Clone, Copy, Serialize)]
pub struct Store {
    pub size_bytes: u64,
    pub entries: usize,
    pub evictions: u64,
}

#[derive(Default)]
struct Inner {
    totals: Counters,
    store: Store,
}

#[derive(Default)]
pub struct Metrics(Mutex<Inner>);

impl Metrics {
    pub fn record(&self, event: Event, bytes: u64) {
        self.0.lock().unwrap().totals.add(event, bytes);
    }

    pub fn record_scan(&self, size: u64, entries: usize, evicted: &[Evicted]) {
        let mut m = self.0.lock().unwrap();
        m.store.size_bytes = size;
        m.store.entries = entries;
        m.store.evictions += evicted.len() as u64;
    }

    pub fn totals(&self) -> (Counters, Store) {
        let m = self.0.lock().unwrap();
        (m.totals, m.store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_totals_and_scans() {
        let m = Metrics::default();
        m.record(Event::Hit, 100);
        m.record(Event::Miss, 0);
        m.record(Event::Upload, 50);
        m.record(Event::Duplicate, 50);
        m.record_scan(10, 1, &[]);

        let (t, store) = m.totals();
        assert_eq!((t.hits, t.misses, t.uploads, t.duplicates), (1, 1, 1, 1));
        assert_eq!((t.bytes_served, t.bytes_received), (100, 100));
        assert_eq!(
            (store.size_bytes, store.entries, store.evictions),
            (10, 1, 0)
        );
    }
}
