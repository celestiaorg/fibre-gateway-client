//! Async client for `/v1/put`, `/v1/get` and `/v1/capacity`, requiring Tokio.

use std::time::Duration;

use bytes::Bytes;
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
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Verify(#[from] VerifyError),
    #[error("verification task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
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
    agent: reqwest::Client,
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
            agent: reqwest::Client::builder()
                .connect_timeout(timeouts.connect)
                .retry(reqwest::retry::never())
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.into(),
            token: token.into(),
            timeouts,
        }
    }

    /// Puts `data` and returns the receipt once its commitment is verified against `data`.
    pub async fn put(&self, data: Bytes) -> Result<Receipt, HttpError> {
        let put: PutResponse = self
            .agent
            .post(format!("{}/v1/put", self.base_url))
            .timeout(self.timeouts.put)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/octet-stream")
            .body(data.clone())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tokio::task::spawn_blocking(move || {
            verify(&data, &put.receipt, &put.commitment_proof)?;
            Ok::<_, VerifyError>(put.receipt)
        })
        .await??)
    }

    /// Gets the blob for `blob_id`.
    pub async fn get(&self, blob_id: &str) -> Result<Vec<u8>, HttpError> {
        let mut res = self
            .agent
            .post(format!("{}/v1/get", self.base_url))
            .timeout(self.timeouts.get)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "blob_id": blob_id }))
            .send()
            .await?
            .error_for_status()?;
        let mut data = Vec::new();
        while data.len() < MAX_DATA_SIZE + 1 {
            let Some(chunk) = res.chunk().await? else {
                break;
            };
            let len = chunk.len().min(MAX_DATA_SIZE + 1 - data.len());
            data.extend_from_slice(&chunk[..len]);
        }
        Ok(data)
    }

    /// Reserves at least `instances` gateway instances (3..=20) for `minutes` (1..=120).
    /// The token must be allowed to reserve capacity.
    pub async fn capacity(
        &self,
        instances: u32,
        minutes: u32,
    ) -> Result<CapacityStatus, HttpError> {
        Ok(self
            .agent
            .post(format!("{}/v1/capacity", self.base_url))
            .timeout(CAPACITY_TIMEOUT)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({"instances": instances, "minutes": minutes}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Returns the current reservation and fleet size.
    pub async fn capacity_status(&self) -> Result<CapacityStatus, HttpError> {
        Ok(self
            .agent
            .get(format!("{}/v1/capacity", self.base_url))
            .timeout(CAPACITY_TIMEOUT)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}
