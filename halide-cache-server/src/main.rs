mod blobs;
mod eviction;
mod extract;
mod metrics;
mod stats;

use axum::{Router, routing::get};
use bytesize::ByteSize;
use clap::Parser;
use lager::Lager;
use metrics::Metrics;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Directory the cache entries are stored in.
    #[arg(long, default_value = "/var/lib/halide-cache")]
    data_dir: PathBuf,
    /// Maximum size of the cache on disk, e.g. 40GiB, 500MB, 1000000.
    #[arg(long, default_value = "40GiB")]
    size: ByteSize,
    /// How often, in seconds, the LRU eviction pass runs.
    #[arg(long, default_value_t = 60)]
    evict_interval: u64,
    /// Largest accepted upload.
    #[arg(long, default_value = "1GiB")]
    max_blob_size: ByteSize,
}

pub struct AppState {
    lager: Lager,
    max_size: u64,
    max_blob_size: u64,
    metrics: Metrics,
    started: Instant,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    std::fs::create_dir_all(&args.data_dir)?;

    let state = Arc::new(AppState {
        lager: Lager::new(&args.data_dir)?,
        max_size: args.size.as_u64(),
        max_blob_size: args.max_blob_size.as_u64(),
        metrics: Metrics::default(),
        started: Instant::now(),
    });

    tokio::spawn(eviction::eviction_loop(
        state.clone(),
        Duration::from_secs(args.evict_interval),
    ));

    let app = Router::new()
        .route("/v1/blobs/{address}", get(blobs::get).put(blobs::put))
        .route("/v1/stats", get(stats::stats))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!(
        listen = %args.listen,
        data_dir = %args.data_dir.display(),
        size = %args.size,
        "halide-cache-server listening"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    info!("shutting down");
}
