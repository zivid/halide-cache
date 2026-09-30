use crate::AppState;
use bytesize::ByteSize;
use lager::LRU;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info};

pub async fn eviction_loop(state: Arc<AppState>, interval: Duration) {
    loop {
        evict(&state).await;
        tokio::time::sleep(interval).await;
    }
}

async fn evict(state: &AppState) {
    let max = state.max_size;
    let target = max / 2;
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
        Ok::<_, lager::Error>((
            before,
            evicted,
            lru.lager_size(),
            lru.entries(),
            lru.largest(),
            lru.oldest(),
        ))
    })
    .await;

    match result {
        Ok(Ok((before, evicted, size, entries, largest, oldest))) => {
            state
                .metrics
                .record_scan(size, entries, largest, oldest, &evicted);
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
