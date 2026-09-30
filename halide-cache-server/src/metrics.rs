use crate::system::Host;
use lager::Evicted;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BUCKET_SECONDS: u64 = 60;
const MAX_BUCKETS: usize = 24 * 60;
const MAX_CLIENTS: usize = 4096;
pub const AGE_BOUNDS: [u64; 6] = [3600, 6 * 3600, 24 * 3600, 3 * 86400, 7 * 86400, u64::MAX];

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Hit,
    Miss,
    Upload,
    Duplicate,
    Rejected,
    Failed,
}

#[derive(Clone, Copy, Debug)]
pub enum Request {
    Get,
    Put,
}

const LATENCY_SAMPLES: usize = 1000;

#[derive(Default, Clone, Copy, Serialize)]
pub struct Counters {
    pub hits: u64,
    pub misses: u64,
    pub uploads: u64,
    pub duplicates: u64,
    pub rejected_uploads: u64,
    pub failed_uploads: u64,
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
            Event::Rejected => self.rejected_uploads += 1,
            Event::Failed => self.failed_uploads += 1,
        }
    }
}

#[derive(Default, Clone, Copy, Serialize)]
pub struct Store {
    pub size_bytes: u64,
    pub entries: usize,
    pub largest_entry_bytes: u64,
    pub oldest_entry_used: Option<u64>,
    pub evictions: u64,
    pub evicted_bytes: u64,
    pub eviction_ages: [u64; 6],
}

#[derive(Default, Clone, Serialize)]
pub struct Bucket {
    pub ts: u64,
    #[serde(flatten)]
    pub counters: Counters,
    pub size_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
pub struct LatencySummary {
    pub median_ms: f64,
    pub p95_ms: f64,
    pub samples: usize,
}

#[derive(Clone, Copy, Serialize)]
pub struct Latencies {
    pub get: Option<LatencySummary>,
    pub put: Option<LatencySummary>,
}

#[derive(Default, Clone, Serialize)]
pub struct ClientStats {
    pub ip: String,
    pub hostname: Option<String>,
    #[serde(flatten)]
    pub counters: Counters,
    pub first_seen: u64,
    pub last_seen: u64,
}

pub struct Client {
    pub ip: IpAddr,
    pub hostname: Option<String>,
}

#[derive(Default)]
struct Inner {
    totals: Counters,
    store: Store,
    host: Host,
    get_latency: VecDeque<Duration>,
    put_latency: VecDeque<Duration>,
    history: VecDeque<Bucket>,
    clients: HashMap<IpAddr, ClientStats>,
}

#[derive(Default)]
pub struct Metrics(Mutex<Inner>);

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Metrics {
    pub fn record(&self, event: Event, client: &Client, bytes: u64) {
        let ts = now();
        let mut m = self.0.lock().unwrap();
        m.totals.add(event, bytes);
        current_bucket(&mut m.history, ts)
            .counters
            .add(event, bytes);

        if m.clients.len() >= MAX_CLIENTS
            && !m.clients.contains_key(&client.ip)
            && let Some(oldest) = m
                .clients
                .iter()
                .min_by_key(|(_, c)| c.last_seen)
                .map(|(ip, _)| *ip)
        {
            m.clients.remove(&oldest);
        }
        let c = m.clients.entry(client.ip).or_insert_with(|| ClientStats {
            ip: client.ip.to_string(),
            first_seen: ts,
            ..Default::default()
        });
        c.last_seen = ts;
        if client.hostname.is_some() {
            c.hostname = client.hostname.clone();
        }
        c.counters.add(event, bytes);
    }

    pub fn record_scan(
        &self,
        size: u64,
        entries: usize,
        largest: u64,
        oldest: Option<SystemTime>,
        evicted: &[Evicted],
    ) {
        let when = SystemTime::now();
        let mut m = self.0.lock().unwrap();
        m.store.size_bytes = size;
        m.store.entries = entries;
        m.store.largest_entry_bytes = largest;
        m.store.oldest_entry_used = oldest
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        for e in evicted {
            let age = when
                .duration_since(e.last_used)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let bucket = AGE_BOUNDS.iter().position(|&b| age < b).unwrap_or(5);
            m.store.eviction_ages[bucket] += 1;
            m.store.evictions += 1;
            m.store.evicted_bytes += e.size;
        }
        current_bucket(&mut m.history, now()).size_bytes = Some(size);
    }

    pub fn totals(&self) -> (Counters, Store) {
        let m = self.0.lock().unwrap();
        (m.totals, m.store)
    }

    pub fn record_host(&self, host: Host) {
        let mut m = self.0.lock().unwrap();
        m.host = host;
        let bucket = current_bucket(&mut m.history, now());
        bucket.cpu_percent = host.cpu_percent;
        bucket.memory_used_bytes = Some(host.memory_total_bytes - host.memory_available_bytes);
    }

    pub fn record_latency(&self, request: Request, elapsed: Duration) {
        let mut m = self.0.lock().unwrap();
        let samples = match request {
            Request::Get => &mut m.get_latency,
            Request::Put => &mut m.put_latency,
        };
        if samples.len() == LATENCY_SAMPLES {
            samples.pop_front();
        }
        samples.push_back(elapsed);
    }

    pub fn latencies(&self) -> Latencies {
        let m = self.0.lock().unwrap();
        Latencies {
            get: summarize(&m.get_latency),
            put: summarize(&m.put_latency),
        }
    }

    pub fn host(&self) -> Host {
        self.0.lock().unwrap().host
    }

    pub fn history(&self, seconds: u64) -> Vec<Bucket> {
        let since = now().saturating_sub(seconds);
        let m = self.0.lock().unwrap();
        m.history
            .iter()
            .filter(|b| b.ts + BUCKET_SECONDS > since)
            .cloned()
            .collect()
    }

    pub fn clients(&self) -> Vec<ClientStats> {
        let mut v: Vec<ClientStats> = self.0.lock().unwrap().clients.values().cloned().collect();
        v.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.ip.cmp(&b.ip)));
        v
    }
}

fn summarize(samples: &VecDeque<Duration>) -> Option<LatencySummary> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<Duration> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let at =
        |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1000.0;
    Some(LatencySummary {
        median_ms: at(0.5),
        p95_ms: at(0.95),
        samples: sorted.len(),
    })
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
        m.record(Event::Hit, &anon, 100);
        m.record(Event::Miss, &named, 0);
        m.record(Event::Upload, &anon, 50);
        m.record(Event::Duplicate, &anon, 50);

        let (t, _) = m.totals();
        assert_eq!((t.hits, t.misses, t.uploads, t.duplicates), (1, 1, 1, 1));
        assert_eq!((t.bytes_served, t.bytes_received), (100, 100));

        let h = m.history(3600);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].counters.hits, h[0].counters.uploads), (1, 1));

        let c = m.clients();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].ip, "10.0.0.7");
        assert_eq!(
            c[0].hostname.as_deref(),
            Some("build-07"),
            "the name sticks"
        );
        assert_eq!(c[0].counters.bytes_received, 100);
    }

    #[test]
    fn eviction_ages_are_bucketed() {
        let m = Metrics::default();
        let evicted = |size, age_secs| Evicted {
            address: lager::Address::from([0u8; 64]),
            last_used: SystemTime::now() - std::time::Duration::from_secs(age_secs),
            size,
        };
        m.record_scan(
            10,
            1,
            300,
            None,
            &[
                evicted(100, 60),
                evicted(200, 7200),
                evicted(300, 30 * 86400),
            ],
        );
        let (_, store) = m.totals();
        assert_eq!(store.eviction_ages, [1, 1, 0, 0, 0, 1]);
        assert_eq!((store.evictions, store.evicted_bytes), (3, 600));
        assert_eq!((store.size_bytes, store.entries), (10, 1));
        assert_eq!(store.largest_entry_bytes, 300);
    }

    #[test]
    fn rejected_and_failed_uploads_are_counted() {
        let m = Metrics::default();
        let client = Client {
            ip: "10.0.0.8".parse().unwrap(),
            hostname: None,
        };
        m.record(Event::Rejected, &client, 0);
        m.record(Event::Failed, &client, 0);
        m.record(Event::Failed, &client, 0);
        let (t, _) = m.totals();
        assert_eq!((t.rejected_uploads, t.failed_uploads, t.uploads), (1, 2, 0));
        assert_eq!(m.clients()[0].counters.failed_uploads, 2);
    }

    #[test]
    fn latency_keeps_the_last_samples_and_their_percentiles() {
        let m = Metrics::default();
        assert!(m.latencies().get.is_none());
        for ms in 1..=(LATENCY_SAMPLES as u64 + 100) {
            m.record_latency(Request::Get, Duration::from_millis(ms));
        }
        let get = m.latencies().get.unwrap();
        assert_eq!(get.samples, LATENCY_SAMPLES);
        assert_eq!(get.median_ms.round(), 601.0);
        assert_eq!(get.p95_ms.round(), 1050.0);
        assert!(m.latencies().put.is_none());
    }

    #[test]
    fn history_is_bounded() {
        let mut history = VecDeque::new();
        for i in 0..(MAX_BUCKETS as u64 + 10) {
            current_bucket(&mut history, i * BUCKET_SECONDS)
                .counters
                .hits += 1;
        }
        assert_eq!(history.len(), MAX_BUCKETS);
        assert_eq!(history.front().unwrap().ts, 10 * BUCKET_SECONDS);
    }
}
