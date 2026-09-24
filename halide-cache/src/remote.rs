//! Client for halide-cache-server. Every failure is reported as an error to the
//! caller, who is expected to log it and continue without the remote: a cache
//! server outage must never fail a build.

use lager::{Address, Kind, Lager};
use std::time::Duration;

const KIND_HEADER: &str = "x-lager-kind";
/// Lets the server show a hostname next to the client address on its dashboard.
const CLIENT_HEADER: &str = "x-halide-cache-client";

pub struct Remote {
    agent: ureq::Agent,
    base_url: String,
    token: Option<String>,
    hostname: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Http(#[from] ureq::Error),
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("uploads rejected: bearer token missing or wrong")]
    Unauthorized,
    #[error("missing or invalid {KIND_HEADER} header")]
    BadKind,
    #[error("{0}")]
    Lager(#[from] lager::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Remote {
    pub fn new(base_url: &str, token: Option<String>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into();
        Remote {
            agent,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        }
    }

    fn url(&self, address: &Address) -> String {
        format!("{}/v1/blobs/{}", self.base_url, address)
    }

    /// Downloads a blob into `lager`. Returns `Ok(false)` when the server does
    /// not have it.
    pub fn fetch_into(&self, address: &Address, lager: &Lager) -> Result<bool> {
        let mut response = self
            .agent
            .get(self.url(address))
            .header(CLIENT_HEADER, &self.hostname)
            .call()?;
        match response.status().as_u16() {
            200 => {}
            404 => return Ok(false),
            s => return Err(Error::Status(s)),
        }
        let kind = response
            .headers()
            .get(KIND_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(Kind::parse)
            .ok_or(Error::BadKind)?;
        let body = response.body_mut().with_config().limit(u64::MAX).reader();
        lager.store_raw(address, kind, body)?;
        Ok(true)
    }

    /// Uploads the blob stored under `address` in `lager`.
    pub fn upload_from(&self, address: &Address, lager: &Lager) -> Result<()> {
        let (file, kind) = lager.open_raw(address)?;
        let len = file.metadata().map_err(lager::Error::from)?.len();

        let mut request = self
            .agent
            .put(self.url(address))
            .header(KIND_HEADER, kind.as_str())
            .header(CLIENT_HEADER, &self.hostname)
            .header("content-length", len.to_string());
        if let Some(token) = &self.token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = request.send(&file)?;
        match response.status().as_u16() {
            200 | 201 => Ok(()),
            401 => Err(Error::Unauthorized),
            s => Err(Error::Status(s)),
        }
    }
}
