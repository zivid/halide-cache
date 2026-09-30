use lager::{Address, Lager};
use std::time::Duration;

const CLIENT_HEADER: &str = "x-halide-cache-client";

pub struct Remote {
    agent: ureq::Agent,
    base_url: String,
    hostname: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Http(#[from] ureq::Error),
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("{0}")]
    Lager(#[from] lager::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Remote {
    pub fn new(base_url: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into();
        Remote {
            agent,
            base_url: base_url.trim_end_matches('/').to_owned(),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        }
    }

    fn url(&self, address: &Address) -> String {
        format!("{}/v1/blobs/{}", self.base_url, address)
    }

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
        let mut body = response.body_mut().with_config().limit(u64::MAX).reader();
        lager.store_at(address, |out| std::io::copy(&mut body, out).map(|_| ()))?;
        Ok(true)
    }

    pub fn upload_from(&self, address: &Address, lager: &Lager) -> Result<()> {
        let file = lager.retrieve(address)?;
        let len = file.metadata().map_err(lager::Error::from)?.len();

        let response = self
            .agent
            .put(self.url(address))
            .header(CLIENT_HEADER, &self.hostname)
            .header("content-length", len.to_string())
            .send(&file)?;
        match response.status().as_u16() {
            200 | 201 => Ok(()),
            s => Err(Error::Status(s)),
        }
    }
}
