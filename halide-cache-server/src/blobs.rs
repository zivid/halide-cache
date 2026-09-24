//! The blob endpoints: `GET`, `HEAD` and `PUT /v1/blobs/{address}`.

use crate::AppState;
use crate::eviction::maybe_evict_after_upload;
use crate::extract::BlobAddress;
use crate::metrics::Event;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use lager::{Address, Kind};
use std::sync::Arc;
use tokio_util::io::{ReaderStream, StreamReader, SyncIoBridge};
use tracing::{error, info, warn};

/// Tells the receiver whether the blob is a compressed file or directory.
pub const KIND_HEADER: &str = "x-lager-kind";

struct Blob {
    file: std::fs::File,
    kind: Kind,
    len: u64,
}

fn internal(err: impl std::fmt::Display) -> StatusCode {
    error!(%err, "internal error");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn open_blob(state: &AppState, address: Address) -> Result<Option<Blob>, StatusCode> {
    let lager = state.lager.clone();
    tokio::task::spawn_blocking(move || match lager.open_raw(&address) {
        Ok((file, kind)) => {
            let len = file.metadata()?.len();
            Ok(Some(Blob { file, kind, len }))
        }
        Err(lager::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e),
    })
    .await
    .map_err(internal)?
    .map_err(internal)
}

fn blob_headers(kind: Kind, len: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(KIND_HEADER, HeaderValue::from_static(kind.as_str()));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    headers
}

pub async fn head(
    State(state): State<Arc<AppState>>,
    BlobAddress(address): BlobAddress,
) -> Response {
    match open_blob(&state, address).await {
        Ok(Some(blob)) => (StatusCode::OK, blob_headers(blob.kind, blob.len)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(status) => status.into_response(),
    }
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    BlobAddress(address): BlobAddress,
) -> Response {
    match open_blob(&state, address).await {
        Ok(Some(blob)) => {
            state.metrics.record(Event::Hit, blob.len);
            info!(%address, kind = blob.kind.as_str(), len = blob.len, "hit");
            let stream = ReaderStream::new(tokio::fs::File::from_std(blob.file));
            (
                StatusCode::OK,
                blob_headers(blob.kind, blob.len),
                Body::from_stream(stream),
            )
                .into_response()
        }
        Ok(None) => {
            state.metrics.record(Event::Miss, 0);
            info!(%address, "miss");
            StatusCode::NOT_FOUND.into_response()
        }
        Err(status) => status.into_response(),
    }
}

pub async fn put(
    State(state): State<Arc<AppState>>,
    BlobAddress(address): BlobAddress,
    request: Request,
) -> Response {
    if !authorized(&state, request.headers()) {
        state.metrics.record(Event::Unauthorized, 0);
        warn!(%address, "unauthorized upload");
        return (StatusCode::UNAUTHORIZED, "bearer token required\n").into_response();
    }
    let Some(kind) = request
        .headers()
        .get(KIND_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(Kind::parse)
    else {
        return (
            StatusCode::BAD_REQUEST,
            format!("{KIND_HEADER} header must be 'file' or 'dir'\n"),
        )
            .into_response();
    };

    // Bridge the async body into the blocking, synchronous lager write. The
    // size limit is enforced here, on the stream: axum's DefaultBodyLimit does
    // not apply to a handler that consumes the raw body.
    let body = request
        .into_body()
        .into_data_stream()
        .map_err(std::io::Error::other);
    let mut reader = CountingReader::new(
        SyncIoBridge::new(StreamReader::new(body)),
        state.max_blob_size,
    );
    let lager = state.lager.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let new = lager.store_raw(&address, kind, &mut reader)?;
        Ok::<_, lager::Error>((new, reader.count))
    })
    .await;

    match stored {
        Ok(Ok((new, len))) => {
            let (event, status, what) = if new {
                (Event::Upload, StatusCode::CREATED, "stored")
            } else {
                (Event::Duplicate, StatusCode::OK, "replaced existing")
            };
            state.metrics.record(event, len);
            info!(%address, kind = kind.as_str(), len, what);
            maybe_evict_after_upload(state.clone());
            status.into_response()
        }
        Ok(Err(lager::Error::Io(e))) if e.kind() == std::io::ErrorKind::FileTooLarge => {
            warn!(%address, "upload rejected: larger than --max-blob-size");
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "blob exceeds --max-blob-size\n",
            )
                .into_response()
        }
        Ok(Err(e)) => {
            // A client that dropped the connection shows up here as an io error
            // while reading the body.
            warn!(%address, err = %e, "upload failed");
            (StatusCode::BAD_REQUEST, "upload failed\n").into_response()
        }
        Err(e) => internal(e).into_response(),
    }
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Counts the bytes that pass through, so the upload size can be logged and
/// accounted for without a second pass, and stops with `FileTooLarge` once
/// `limit` is exceeded.
struct CountingReader<R> {
    inner: R,
    count: u64,
    limit: u64,
}

impl<R> CountingReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        CountingReader {
            inner,
            count: 0,
            limit,
        }
    }
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        if self.count > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "blob exceeds the configured maximum size",
            ));
        }
        Ok(n)
    }
}
