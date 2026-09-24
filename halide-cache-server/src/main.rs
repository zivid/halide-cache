//! HTTP server sharing halide-cache entries between machines.
//!
//! Blobs are stored with the same on-disk layout as the local client cache
//! (see the `lager` crate), so the client can move compressed blobs back and
//! forth without recompressing them.

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
use tracing::{info, warn};

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
    /// Bearer token required for uploads. Reads are always allowed. When unset,
    /// uploads are allowed without authentication (see --require-token).
    #[arg(long, env = "HALIDE_CACHE_SERVER_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Refuse to start unless a non-empty --token is configured. For deployments
    /// where an open server would be a misconfiguration, such as the bundled
    /// systemd unit.
    #[arg(long)]
    require_token: bool,
    /// How often, in seconds, the LRU eviction pass runs.
    #[arg(long, default_value_t = 300)]
    evict_interval: u64,
    /// Largest accepted upload.
    #[arg(long, default_value = "1GiB")]
    max_blob_size: ByteSize,
}

pub struct AppState {
    lager: Lager,
    max_size: u64,
    max_blob_size: u64,
    token: Option<String>,
    metrics: Metrics,
    /// Held by the running eviction pass; see `eviction::evict`.
    evict_lock: tokio::sync::Mutex<()>,
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

    let token = args.token.filter(|t| !t.is_empty());
    if args.require_token && token.is_none() {
        anyhow::bail!(
            "--require-token is set but no token is configured (--token or HALIDE_CACHE_SERVER_TOKEN)"
        );
    }

    let state = Arc::new(AppState {
        lager: Lager::new(&args.data_dir)?,
        max_size: args.size.as_u64(),
        max_blob_size: args.max_blob_size.as_u64(),
        token,
        metrics: Metrics::default(),
        evict_lock: tokio::sync::Mutex::new(()),
        started: Instant::now(),
    });

    if state.token.is_none() {
        warn!("no --token configured: anybody can upload");
    }

    tokio::spawn(eviction::eviction_loop(
        state.clone(),
        Duration::from_secs(args.evict_interval),
    ));

    let app = Router::new()
        .route("/", get(stats::dashboard))
        .route(
            "/v1/blobs/{address}",
            get(blobs::get).head(blobs::head).put(blobs::put),
        )
        .route("/v1/stats", get(stats::stats))
        .route("/v1/history", get(stats::history))
        .route("/v1/clients", get(stats::clients))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!(
        listen = %args.listen,
        data_dir = %args.data_dir.display(),
        size = %args.size,
        "halide-cache-server listening"
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
