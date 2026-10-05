//! Blocking client for `/v1/put`, `/v1/get` and `/v1/capacity`.

use std::io::Read;
use std::time::Duration;

use serde::Deserialize;

use crate::{verify, PutResponse, Receipt, VerifyError, MAX_DATA_SIZE};

/// Whole capacity request. The gateway answers within about 5 s.
const CAPACITY_TIMEOUT: Duration = Duration::from_secs(30);

/// The gateway fleet's capacity floor and size.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CapacityStatus {
    /// Reserved instances; 0 when no reservation is active.
    pub floor: u32,
    /// When the reservation ends (RFC 3339), if one is active.
    pub expires_at: Option<String>,
    /// Instances the fleet is scaling to.
    pub target: u32,
    /// Instances serving traffic now.
    pub active: u32,
    /// Instances running, including ones still warming up.
    pub running: u32,
    /// Estimated seconds until `target` instances serve traffic.
    pub eta_seconds: u64,
}

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request failed: {0}")]
    Request(Box<ureq::Error>),
    #[error("reading response: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Verify(#[from] VerifyError),
}

impl From<ureq::Error> for HttpError {
    fn from(err: ureq::Error) -> Self {
        Self::Request(Box::new(err))
    }
}

/// Request timeouts. The defaults outlast the gateway's own deadlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Connecting, including the TLS handshake. Default 10 s.
    pub connect: Duration,
    /// Whole put request. Default 160 s.
    pub put: Duration,
    /// Whole get request, including reading the blob. Default 130 s.
    pub get: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            put: Duration::from_secs(160),
            get: Duration::from_secs(130),
        }
    }
}

/// Gateway client. `base_url` is like `https://cf.celestia-corto.com:8443`.
pub struct Client {
    agent: ureq::Agent,
    base_url: String,
    token: String,
    timeouts: Timeouts,
}

impl Client {
    /// Client with the default [`Timeouts`].
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_timeouts(base_url, token, Timeouts::default())
    }

    /// Client with custom timeouts.
    pub fn with_timeouts(
        base_url: impl Into<String>,
        token: impl Into<String>,
        timeouts: Timeouts,
    ) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(timeouts.connect)
                .build(),
            base_url: base_url.into(),
            token: token.into(),
            timeouts,
        }
    }

    /// Puts `data` and returns the receipt once its commitment is verified against `data`.
    pub fn put(&self, data: &[u8]) -> Result<Receipt, HttpError> {
        let put: PutResponse = self
            .agent
            .post(&format!("{}/v1/put", self.base_url))
            .timeout(self.timeouts.put)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/octet-stream")
            .send_bytes(data)?
            .into_json()?;
        verify(data, &put.receipt, &put.commitment_proof)?;
        Ok(put.receipt)
    }

    /// Gets the blob for `blob_id`.
    pub fn get(&self, blob_id: &str) -> Result<Vec<u8>, HttpError> {
        let res = self
            .agent
            .post(&format!("{}/v1/get", self.base_url))
            .timeout(self.timeouts.get)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(serde_json::json!({ "blob_id": blob_id }))?;
        let mut data = Vec::new();
        res.into_reader()
            .take(MAX_DATA_SIZE as u64 + 1)
            .read_to_end(&mut data)?;
        Ok(data)
    }

    /// Reserves at least `instances` gateway instances (3..=20) for `minutes` (1..=120).
    /// The token must be allowed to reserve capacity.
    pub fn capacity(&self, instances: u32, minutes: u32) -> Result<CapacityStatus, HttpError> {
        Ok(self
            .agent
            .post(&format!("{}/v1/capacity", self.base_url))
            .timeout(CAPACITY_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(serde_json::json!({"instances": instances, "minutes": minutes}))?
            .into_json()?)
    }

    /// Returns the current reservation and fleet size.
    pub fn capacity_status(&self) -> Result<CapacityStatus, HttpError> {
        Ok(self
            .agent
            .get(&format!("{}/v1/capacity", self.base_url))
            .timeout(CAPACITY_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()?
            .into_json()?)
    }
}
