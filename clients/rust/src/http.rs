//! Async client for `/v1/put`, `/v1/get` and `/v1/capacity`, requiring Tokio.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER};
use reqwest::{Response, Url};
use serde::Deserialize;

use crate::{verify, PutResponse, Receipt, VerifyError, MAX_DATA_SIZE};

/// Most of an error body read; gateway error bodies are a few dozen bytes.
const MAX_ERROR_BODY: usize = 64 << 10;

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

/// A non-2xx response from the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// HTTP status code.
    pub status: u16,
    /// Stable error code from the gateway's JSON body, like `capacity` or
    /// `invalid_length`. `None` when the body is not the gateway's JSON, e.g. an
    /// error page from a proxy or load balancer in front of it.
    pub code: Option<String>,
    /// Human-readable hint from the gateway. `None` when there is no gateway JSON
    /// body or the gateway gives no hint for this error, which is most of them.
    pub message: Option<String>,
    /// Wait this long before retrying, from the `Retry-After` header; `None`
    /// when the header is absent.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gateway returned {}", self.status)?;
        if let Some(code) = &self.code {
            write!(f, " {code}")?;
        }
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        if let Some(retry_after) = self.retry_after {
            write!(f, " (retry after {} s)", retry_after.as_secs())?;
        }
        Ok(())
    }
}

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The gateway answered with a non-2xx status.
    #[error("{0}")]
    Status(ApiError),
    #[error(transparent)]
    Verify(#[from] VerifyError),
    #[error("verification task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl HttpError {
    /// The HTTP status, if the gateway answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status(e) => Some(e.status),
            _ => None,
        }
    }
}

/// Turns a non-2xx response into [`HttpError::Status`], keeping the body's
/// `error` and `message` and the `Retry-After` header.
async fn check(mut res: Response) -> Result<Response, HttpError> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let retry_after = res
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_secs);
    /// A body that is not the gateway's JSON parses as empty.
    #[derive(Default, Deserialize)]
    struct Body {
        error: Option<String>,
        message: Option<String>,
    }
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BODY {
        let Ok(Some(chunk)) = res.chunk().await else {
            break;
        };
        bytes.extend_from_slice(&chunk[..chunk.len().min(MAX_ERROR_BODY - bytes.len())]);
    }
    let body: Body = serde_json::from_slice(&bytes).unwrap_or_default();
    Err(HttpError::Status(ApiError {
        status: status.as_u16(),
        code: body.error,
        message: body.message,
        retry_after,
    }))
}

/// Resolves names with the system resolver and rotates the address list on
/// every lookup. The gateway name has one address per instance, and a connection
/// goes to the first address that answers. The system resolver usually returns
/// the same order every time, so without rotation every connection would land
/// on the same instance.
#[derive(Default)]
struct RotatingResolver {
    next: AtomicUsize,
}

impl Resolve for RotatingResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            Ok(Box::new(rotate(addrs, start).into_iter()) as Addrs)
        })
    }
}

fn rotate(mut addrs: Vec<SocketAddr>, start: usize) -> Vec<SocketAddr> {
    if !addrs.is_empty() {
        let n = start % addrs.len();
        addrs.rotate_left(n);
    }
    addrs
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
    put_url: Url,
    get_url: Url,
    capacity_url: Url,
    timeouts: Timeouts,
}

impl Client {
    /// Client with the default [`Timeouts`].
    ///
    /// # Panics
    /// Panics if the URL or authorization header is invalid, or HTTP client initialization fails.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_timeouts(base_url, token, Timeouts::default())
    }

    /// Client with custom timeouts.
    ///
    /// # Panics
    /// Panics if the URL or authorization header is invalid, or HTTP client initialization fails.
    pub fn with_timeouts(
        base_url: impl Into<String>,
        token: impl Into<String>,
        timeouts: Timeouts,
    ) -> Self {
        let base_url = base_url.into();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", token.into()))
            .expect("invalid authorization header");
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);

        Self {
            agent: reqwest::Client::builder()
                .default_headers(headers)
                .connect_timeout(timeouts.connect)
                .retry(reqwest::retry::never())
                // A 307/308 would resend the put body and could store the blob twice.
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(Arc::new(RotatingResolver::default()))
                // HTTP/2 would multiplex every request over one connection, so one
                // instance, even if a dependency enables reqwest's http2 feature.
                .http1_only()
                .build()
                .expect("failed to build HTTP client"),
            put_url: Url::parse(&format!("{base_url}/v1/put")).expect("invalid gateway URL"),
            get_url: Url::parse(&format!("{base_url}/v1/get")).expect("invalid gateway URL"),
            capacity_url: Url::parse(&format!("{base_url}/v1/capacity"))
                .expect("invalid gateway URL"),
            timeouts,
        }
    }

    /// Puts `data` and returns the receipt once its commitment is verified against `data`.
    pub async fn put(&self, data: Bytes) -> Result<Receipt, HttpError> {
        let put: PutResponse = check(
            self.agent
                .post(self.put_url.clone())
                .timeout(self.timeouts.put)
                .header("Content-Type", "application/octet-stream")
                .body(data.clone())
                .send()
                .await?,
        )
        .await?
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
        let mut res = check(
            self.agent
                .post(self.get_url.clone())
                .timeout(self.timeouts.get)
                .json(&serde_json::json!({ "blob_id": blob_id }))
                .send()
                .await?,
        )
        .await?;
        let capacity = res
            .content_length()
            .unwrap_or(0)
            .min((MAX_DATA_SIZE + 1) as u64) as usize;
        let mut data = Vec::with_capacity(capacity);
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
        Ok(check(
            self.agent
                .post(self.capacity_url.clone())
                .timeout(CAPACITY_TIMEOUT)
                .json(&serde_json::json!({"instances": instances, "minutes": minutes}))
                .send()
                .await?,
        )
        .await?
        .json()
        .await?)
    }

    /// Returns the current reservation and fleet size.
    pub async fn capacity_status(&self) -> Result<CapacityStatus, HttpError> {
        Ok(check(
            self.agent
                .get(self.capacity_url.clone())
                .timeout(CAPACITY_TIMEOUT)
                .send()
                .await?,
        )
        .await?
        .json()
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_starts_at_a_different_address_each_time() {
        let addrs: Vec<SocketAddr> = (1..=3)
            .map(|i| SocketAddr::from(([10, 0, 0, i], 0)))
            .collect();
        let firsts: Vec<_> = (0..4).map(|n| rotate(addrs.clone(), n)[0]).collect();
        assert_eq!(firsts, [addrs[0], addrs[1], addrs[2], addrs[0]]);
        // Every address stays in the list, so a refused connection falls back to the next.
        assert_eq!(rotate(addrs.clone(), 1), [addrs[1], addrs[2], addrs[0]]);
        assert!(rotate(Vec::new(), 5).is_empty());
    }
}
