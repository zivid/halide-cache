//! Read-only endpoints: JSON statistics for the dashboard, Prometheus text,
//! and the dashboard page itself.

use crate::AppState;
use crate::metrics::{self, AGE_BOUNDS, Totals};
use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Json, Response},
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub async fn dashboard() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

#[derive(serde::Serialize)]
pub struct Stats {
    uptime_seconds: u64,
    capacity_bytes: u64,
    size_bytes: u64,
    entries: usize,
    hits: u64,
    misses: u64,
    hit_rate: f64,
    uploads: u64,
    duplicates: u64,
    unauthorized: u64,
    evictions: u64,
    bytes_served: u64,
    bytes_received: u64,
    eviction_ages: metrics::EvictionAges,
    eviction_age_bounds_seconds: [u64; 6],
}

fn load(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

pub async fn stats(State(state): State<Arc<AppState>>) -> Json<Stats> {
    let t: &Totals = &state.metrics.totals;
    let (hits, misses) = (load(&t.hits), load(&t.misses));
    let (size_bytes, entries) = state.metrics.last_scan();
    Json(Stats {
        uptime_seconds: state.started.elapsed().as_secs(),
        capacity_bytes: state.max_size,
        size_bytes,
        entries,
        hits,
        misses,
        hit_rate: if hits + misses == 0 {
            0.0
        } else {
            hits as f64 / (hits + misses) as f64
        },
        uploads: load(&t.uploads),
        duplicates: load(&t.duplicates),
        unauthorized: load(&t.unauthorized),
        evictions: load(&t.evictions),
        bytes_served: load(&t.bytes_served),
        bytes_received: load(&t.bytes_received),
        eviction_ages: state.metrics.eviction_ages(),
        eviction_age_bounds_seconds: AGE_BOUNDS,
    })
}

#[derive(serde::Deserialize)]
pub struct HistoryQuery {
    /// Window in seconds, at most 24 hours.
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

/// Prometheus text exposition format, for scraping.
pub async fn prometheus(State(state): State<Arc<AppState>>) -> Response {
    let t = &state.metrics.totals;
    let (size, entries) = state.metrics.last_scan();
    let ages = state.metrics.eviction_ages();

    let gauges = [
        (
            "capacity_bytes",
            "Configured maximum cache size",
            state.max_size,
        ),
        ("size_bytes", "Cache size at the last eviction pass", size),
        (
            "entries",
            "Entries at the last eviction pass",
            entries as u64,
        ),
        (
            "uptime_seconds",
            "Seconds since start",
            state.started.elapsed().as_secs(),
        ),
    ];
    let counters = [
        (
            "hits_total",
            "Blob downloads that found an entry",
            load(&t.hits),
        ),
        (
            "misses_total",
            "Blob downloads that found nothing",
            load(&t.misses),
        ),
        ("uploads_total", "New entries uploaded", load(&t.uploads)),
        (
            "duplicate_uploads_total",
            "Uploads replacing an existing entry",
            load(&t.duplicates),
        ),
        (
            "unauthorized_uploads_total",
            "Uploads rejected for a missing or wrong token",
            load(&t.unauthorized),
        ),
        (
            "evictions_total",
            "Entries removed by the LRU",
            load(&t.evictions),
        ),
        (
            "evicted_bytes_total",
            "Bytes removed by the LRU",
            ages.bytes_evicted,
        ),
        (
            "served_bytes_total",
            "Bytes sent to clients",
            load(&t.bytes_served),
        ),
        (
            "received_bytes_total",
            "Bytes received from clients",
            load(&t.bytes_received),
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

    out.push_str(
        "# HELP halide_cache_eviction_age_seconds Seconds since last use when an entry was evicted\n\
         # TYPE halide_cache_eviction_age_seconds histogram\n",
    );
    let mut cumulative = 0;
    for (count, bound) in ages.buckets.iter().zip(AGE_BOUNDS) {
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
