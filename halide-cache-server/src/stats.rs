use crate::AppState;
use crate::metrics::{self, AGE_BOUNDS, Counters, Latencies, Store};
use crate::system::Host;
use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Json, Response},
};
use std::sync::Arc;

pub async fn dashboard() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

#[derive(serde::Serialize)]
pub struct Stats {
    uptime_seconds: u64,
    capacity_bytes: u64,
    hit_rate: f64,
    #[serde(flatten)]
    totals: Counters,
    #[serde(flatten)]
    store: Store,
    average_entry_bytes: u64,
    eviction_age_bounds_seconds: [u64; 6],
    host: Host,
    latency: Latencies,
}

fn average_entry_bytes(store: &Store) -> u64 {
    store.size_bytes / (store.entries as u64).max(1)
}

pub async fn stats(State(state): State<Arc<AppState>>) -> Json<Stats> {
    let (totals, store) = state.metrics.totals();
    let lookups = totals.hits + totals.misses;
    Json(Stats {
        uptime_seconds: state.started.elapsed().as_secs(),
        capacity_bytes: state.max_size,
        hit_rate: if lookups == 0 {
            0.0
        } else {
            totals.hits as f64 / lookups as f64
        },
        totals,
        store,
        average_entry_bytes: average_entry_bytes(&store),
        eviction_age_bounds_seconds: AGE_BOUNDS,
        host: state.metrics.host(),
        latency: state.metrics.latencies(),
    })
}

#[derive(serde::Deserialize)]
pub struct HistoryQuery {
    #[serde(default = "default_window")]
    seconds: u64,
}

fn default_window() -> u64 {
    3600
}

#[derive(serde::Serialize)]
pub struct History {
    bucket_seconds: u64,
    now: u64,
    buckets: Vec<metrics::Bucket>,
}

pub async fn history(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<History> {
    Json(History {
        bucket_seconds: metrics::BUCKET_SECONDS,
        now: metrics::now(),
        buckets: state.metrics.history(q.seconds.min(24 * 3600)),
    })
}

pub async fn clients(State(state): State<Arc<AppState>>) -> Json<Vec<metrics::ClientStats>> {
    Json(state.metrics.clients())
}

pub async fn prometheus(State(state): State<Arc<AppState>>) -> Response {
    let (t, store) = state.metrics.totals();

    let gauges = [
        (
            "capacity_bytes",
            "Configured maximum cache size",
            state.max_size,
        ),
        (
            "size_bytes",
            "Cache size at the last eviction pass",
            store.size_bytes,
        ),
        (
            "entries",
            "Entries at the last eviction pass",
            store.entries as u64,
        ),
        (
            "largest_entry_bytes",
            "Largest entry at the last eviction pass",
            store.largest_entry_bytes,
        ),
        (
            "average_entry_bytes",
            "Average entry size at the last eviction pass",
            average_entry_bytes(&store),
        ),
        (
            "uptime_seconds",
            "Seconds since start",
            state.started.elapsed().as_secs(),
        ),
    ];
    let counters = [
        ("hits_total", "Downloads that found an entry", t.hits),
        ("misses_total", "Downloads that found nothing", t.misses),
        ("uploads_total", "New entries uploaded", t.uploads),
        (
            "duplicate_uploads_total",
            "Uploads replacing an existing entry",
            t.duplicates,
        ),
        (
            "evictions_total",
            "Entries removed by the LRU",
            store.evictions,
        ),
        (
            "evicted_bytes_total",
            "Bytes removed by the LRU",
            store.evicted_bytes,
        ),
        (
            "served_bytes_total",
            "Bytes sent to clients",
            t.bytes_served,
        ),
        (
            "received_bytes_total",
            "Bytes received from clients",
            t.bytes_received,
        ),
        (
            "rejected_uploads_total",
            "Uploads rejected for exceeding --max-blob-size",
            t.rejected_uploads,
        ),
        (
            "failed_uploads_total",
            "Uploads that failed, typically a dropped connection",
            t.failed_uploads,
        ),
    ];

    let mut out = String::new();
    for (kind, metrics) in [("gauge", &gauges[..]), ("counter", &counters[..])] {
        for (name, help, value) in metrics {
            out.push_str(&format!(
                "# HELP halide_cache_{name} {help}\n# TYPE halide_cache_{name} {kind}\nhalide_cache_{name} {value}\n"
            ));
        }
    }

    let host = state.metrics.host();
    let mut host_gauges = vec![
        ("host_cpus", "Logical CPUs of the host", host.cpus as f64),
        (
            "host_load1",
            "Host load average over 1 minute",
            host.load[0],
        ),
        (
            "host_load5",
            "Host load average over 5 minutes",
            host.load[1],
        ),
        (
            "host_load15",
            "Host load average over 15 minutes",
            host.load[2],
        ),
        (
            "host_memory_total_bytes",
            "Host memory",
            host.memory_total_bytes as f64,
        ),
        (
            "host_memory_available_bytes",
            "Host memory available for new allocations",
            host.memory_available_bytes as f64,
        ),
    ];
    host_gauges.push((
        "disk_total_bytes",
        "Size of the filesystem holding the store",
        host.disk_total_bytes as f64,
    ));
    host_gauges.push((
        "disk_available_bytes",
        "Free space on the filesystem holding the store",
        host.disk_available_bytes as f64,
    ));
    if let Some(used) = store.oldest_entry_used {
        host_gauges.push((
            "oldest_entry_age_seconds",
            "Time since the least recently used entry was last used",
            metrics::now().saturating_sub(used) as f64,
        ));
    }
    let latency = state.metrics.latencies();
    if let Some(get) = latency.get {
        host_gauges.push((
            "get_latency_median_ms",
            "Median time to answer a download",
            get.median_ms,
        ));
        host_gauges.push((
            "get_latency_p95_ms",
            "95th percentile time to answer a download",
            get.p95_ms,
        ));
    }
    if let Some(put) = latency.put {
        host_gauges.push((
            "put_latency_median_ms",
            "Median time to store an upload",
            put.median_ms,
        ));
        host_gauges.push((
            "put_latency_p95_ms",
            "95th percentile time to store an upload",
            put.p95_ms,
        ));
    }
    if let Some(cpu) = host.cpu_percent {
        host_gauges.push((
            "host_cpu_percent",
            "Host CPU usage over the last 5 seconds",
            cpu,
        ));
    }
    for (name, help, value) in host_gauges {
        out.push_str(&format!(
            "# HELP halide_cache_{name} {help}\n# TYPE halide_cache_{name} gauge\nhalide_cache_{name} {value}\n"
        ));
    }

    out.push_str(
        "# HELP halide_cache_eviction_age_seconds Seconds since last use when an entry was evicted\n\
         # TYPE halide_cache_eviction_age_seconds histogram\n",
    );
    let mut cumulative = 0;
    for (count, bound) in store.eviction_ages.iter().zip(AGE_BOUNDS) {
        cumulative += count;
        let le = if bound == u64::MAX {
            "+Inf".to_string()
        } else {
            bound.to_string()
        };
        out.push_str(&format!(
            "halide_cache_eviction_age_seconds_bucket{{le=\"{le}\"}} {cumulative}\n"
        ));
    }
    out.push_str(&format!(
        "halide_cache_eviction_age_seconds_count {cumulative}\n"
    ));

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}
