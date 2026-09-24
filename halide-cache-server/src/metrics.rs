//! In-memory request metrics for /v1/stats, /v1/history and /v1/clients. Everything here is lost on restart by design: the numbers are
//! operational, not billing.

use lager::Evicted;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One-minute buckets, kept for 24 hours.
pub const BUCKET_SECONDS: u64 = 60;
const MAX_BUCKETS: usize = 24 * 60;
/// Upper bound on distinct client addresses we track.
const MAX_CLIENTS: usize = 4096;

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

#[derive(Default, Clone, serde::Serialize)]
pub struct ClientStats {
    pub ip: String,
    /// Hostname reported by the client, if any; the most recent one wins.
    pub hostname: Option<String>,
    pub hits: u64,
    pub misses: u64,
    pub uploads: u64,
    pub unauthorized: u64,
    pub bytes_served: u64,
    pub bytes_received: u64,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// Who made a request, for the client table.
pub struct Client {
    pub ip: IpAddr,
    pub hostname: Option<String>,
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
    clients: Mutex<HashMap<IpAddr, ClientStats>>,
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
/// to the totals, the current history bucket and the client's row alike.
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
    pub fn record(&self, event: Event, client: Option<&Client>, bytes: u64) {
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

        if let Some(Client { ip, hostname }) = client {
            let mut clients = self.clients.lock().unwrap();
            if clients.len() >= MAX_CLIENTS && !clients.contains_key(ip) {
                // Drop the least recently seen client to make room.
                if let Some(oldest) = clients
                    .iter()
                    .min_by_key(|(_, c)| c.last_seen)
                    .map(|(ip, _)| *ip)
                {
                    clients.remove(&oldest);
                }
            }
            let c = clients.entry(*ip).or_insert_with(|| ClientStats {
                ip: ip.to_string(),
                first_seen: ts,
                ..Default::default()
            });
            c.last_seen = ts;
            if hostname.is_some() {
                c.hostname = hostname.clone();
            }
            c.hits += d.hits;
            c.misses += d.misses;
            c.uploads += d.uploads;
            c.unauthorized += d.unauthorized;
            c.bytes_served += d.bytes_served;
            c.bytes_received += d.bytes_received;
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

    /// Clients, most recently active first.
    pub fn clients(&self) -> Vec<ClientStats> {
        let mut v: Vec<ClientStats> = self.clients.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.ip.cmp(&b.ip)));
        v
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
    fn records_totals_buckets_and_clients() {
        let m = Metrics::default();
        let ip: IpAddr = "10.0.0.7".parse().unwrap();
        let anon = Client { ip, hostname: None };
        let named = Client {
            ip,
            hostname: Some("build-07".into()),
        };
        m.record(Event::Hit, Some(&anon), 100);
        m.record(Event::Miss, Some(&named), 0);
        m.record(Event::Upload, Some(&anon), 50);
        m.record(Event::Duplicate, None, 50);

        assert_eq!(m.totals.hits.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.uploads.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.duplicates.load(Ordering::Relaxed), 1);
        assert_eq!(m.totals.bytes_received.load(Ordering::Relaxed), 100);

        let h = m.history(3600);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].hits, 1);
        assert_eq!(h[0].uploads, 2);
        assert_eq!(h[0].bytes_served, 100);

        let c = m.clients();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].ip, "10.0.0.7");
        assert_eq!(c[0].hostname.as_deref(), Some("build-07"));
        assert_eq!(c[0].uploads, 1);
        assert_eq!(c[0].bytes_received, 50);
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
