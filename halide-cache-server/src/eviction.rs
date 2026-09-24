//! Keeping the store under `--size` with the LRU from `lager`.

use crate::AppState;
use bytesize::ByteSize;
use lager::LRU;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{error, info};

pub async fn eviction_loop(state: Arc<AppState>, interval: Duration) {
    loop {
        evict(&state).await;
        tokio::time::sleep(interval).await;
    }
}

/// Evict opportunistically once uploads since the last pass exceed 5 % of the
/// capacity, so a burst of uploads cannot overshoot the limit by much before
/// the periodic pass runs.
pub fn maybe_evict_after_upload(state: Arc<AppState>) {
    let threshold = state.max_size / 20;
    let uploaded = state
        .metrics
        .totals
        .bytes_since_evict
        .load(Ordering::Relaxed);
    if uploaded > threshold {
        tokio::spawn(async move { evict(&state).await });
    }
}

/// Shrinks the cache to below `--size`. Only one pass runs at a time; a pass
/// requested while another is running is skipped since the running one will
/// account for the new uploads anyway.
async fn evict(state: &Arc<AppState>) {
    let Ok(_guard) = state.evict_lock.try_lock() else {
        return;
    };
    // Reset *before* scanning: uploads that land during the scan may be missed
    // by it, so their bytes must survive to trigger the next pass.
    state
        .metrics
        .totals
        .bytes_since_evict
        .store(0, Ordering::Relaxed);

    let max = state.max_size;
    // Evict down to a low-water mark so that a full cache does not trigger an
    // eviction pass on every single upload.
    let target = max - max / 20;
    let lager = state.lager.clone();
    let started = Instant::now();

    let result = tokio::task::spawn_blocking(move || {
        let mut lru = LRU::new(lager);
        lru.scan()?;
        let before = lru.lager_size();
        let evicted = if before > max {
            lru.evict_until(target)?
        } else {
            Vec::new()
        };
        Ok::<_, lager::Error>((before, evicted, lru.lager_size(), lru.entries()))
    })
    .await;

    match result {
        Ok(Ok((before, evicted, size, entries))) => {
            state.metrics.record_scan(size, entries, &evicted);
            info!(
                size = %ByteSize::b(size),
                entries,
                evicted = evicted.len(),
                freed = %ByteSize::b(before.saturating_sub(size)),
                took_ms = started.elapsed().as_millis(),
                "eviction pass"
            );
        }
        Ok(Err(e)) => error!(err = %e, "eviction pass failed"),
        Err(e) => error!(err = %e, "eviction task panicked"),
    }
}
