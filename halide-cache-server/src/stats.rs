use crate::AppState;
use crate::metrics::{Counters, Store};
use axum::{extract::State, response::Json};
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct Stats {
    uptime_seconds: u64,
    capacity_bytes: u64,
    hit_rate: f64,
    #[serde(flatten)]
    totals: Counters,
    #[serde(flatten)]
    store: Store,
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
    })
}
