//! Read-only endpoints: JSON statistics.

use crate::AppState;
use crate::metrics::{self, Totals};
use axum::{
    extract::{Query, State},
    response::Json,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
