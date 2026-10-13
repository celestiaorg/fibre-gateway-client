//! Async client for `/v1/put`, `/v1/get` and `/v1/capacity`, requiring Tokio.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER};
use reqwest::{Response, Url};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

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
    /// Resolving the gateway name to instance addresses failed (HTTP/2 only).
    #[error("resolving the gateway failed: {0}")]
    Resolve(#[from] std::io::Error),
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

/// How the client reaches the gateway fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// HTTP/1.1 with the [`RotatingResolver`]: one connection per in-flight
    /// request, each landing on a different instance. The default; it spreads
    /// load without an upload penalty.
    #[default]
    Http1,
    /// HTTP/2 with one multiplexed connection per instance. Resolves the fleet,
    /// keeps a connection to every instance, sends each request to the least-busy
    /// one and re-resolves about once a minute. Large uploads can be slower than
    /// HTTP/1.1 because the server controls the per-stream flow-control window.
    Http2,
}

/// Client options. Start from the default and override what you need.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Request timeouts.
    pub timeouts: Timeouts,
    /// Transport protocol and load-spreading strategy. Default [`Protocol::Http1`].
    pub protocol: Protocol,
}

/// Re-resolve the fleet about this often; matches the DNS TTL.
const REFRESH_EVERY: Duration = Duration::from_secs(60);
/// Skip an instance for this long after it fails to connect.
const CONNECT_COOLDOWN: Duration = Duration::from_secs(30);
/// Try at most this many instances for one request after connect failures.
const MAX_CONNECT_ATTEMPTS: usize = 3;

/// One gateway instance in the HTTP/2 [`Pool`].
struct Instance {
    addr: SocketAddr,
    /// reqwest client pinned to `addr`; TLS and SNI still use the hostname.
    agent: reqwest::Client,
    /// Requests currently in flight to this instance.
    in_flight: AtomicUsize,
    /// Skip this instance until this instant after a connect failure.
    cooldown: Mutex<Option<Instant>>,
}

impl Instance {
    fn load(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    fn cooling(&self, now: Instant) -> bool {
        self.cooldown
            .lock()
            .unwrap()
            .is_some_and(|until| until > now)
    }

    fn mark_failed(&self) {
        *self.cooldown.lock().unwrap() = Some(Instant::now() + CONNECT_COOLDOWN);
    }
}

/// Counts one in-flight request to an instance and decrements it on drop, so
/// [`select`] keeps steering new requests to the least-busy instance.
struct InFlight(Arc<Instance>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Least-busy instance not in cooldown, or the least-busy overall when every
/// instance is cooling. `exclude` holds addresses already tried this request.
fn select(
    instances: &[Arc<Instance>],
    now: Instant,
    exclude: &[SocketAddr],
) -> Option<Arc<Instance>> {
    let pick = |allow_cooling: bool| {
        instances
            .iter()
            .filter(|i| !exclude.contains(&i.addr))
            .filter(|i| allow_cooling || !i.cooling(now))
            .min_by_key(|i| i.load())
            .cloned()
    };
    pick(false).or_else(|| pick(true))
}

/// The instance list for `addrs`, reusing existing instances (and their live
/// connections) by address and dropping ones no longer resolved.
fn reconcile(
    existing: &[Arc<Instance>],
    addrs: &[SocketAddr],
    make: impl Fn(SocketAddr) -> Arc<Instance>,
) -> Vec<Arc<Instance>> {
    addrs
        .iter()
        .map(|&addr| {
            existing
                .iter()
                .find(|i| i.addr == addr)
                .cloned()
                .unwrap_or_else(|| make(addr))
        })
        .collect()
}

/// A pool of per-instance HTTP/2 clients for one gateway host. Each instance has
/// its own connection, so multiplexing no longer pins all traffic to one
/// instance. The list is refreshed from DNS about once a minute.
struct Pool {
    hostname: String,
    port: u16,
    headers: HeaderMap,
    connect_timeout: Duration,
    created: Instant,
    /// Milliseconds since `created` of the last successful resolve.
    last_refresh: AtomicU64,
    instances: RwLock<Vec<Arc<Instance>>>,
    /// Serializes refreshes so a burst triggers one resolve, not one each.
    refresh_gate: AsyncMutex<()>,
}

impl Pool {
    fn new(hostname: String, port: u16, headers: HeaderMap, connect_timeout: Duration) -> Self {
        Self {
            hostname,
            port,
            headers,
            connect_timeout,
            created: Instant::now(),
            last_refresh: AtomicU64::new(0),
            instances: RwLock::new(Vec::new()),
            refresh_gate: AsyncMutex::new(()),
        }
    }

    /// A client pinned to `addr` over HTTP/2. `resolve_to_addrs` keeps TLS and
    /// SNI on the hostname, and the adaptive window lifts the download flow-control
    /// limit that would otherwise throttle large gets.
    fn make_instance(&self, addr: SocketAddr) -> Arc<Instance> {
        let agent = reqwest::Client::builder()
            .default_headers(self.headers.clone())
            .connect_timeout(self.connect_timeout)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&self.hostname, &[addr])
            .http2_prior_knowledge()
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .build()
            .expect("failed to build HTTP/2 client");
        Arc::new(Instance {
            addr,
            agent,
            in_flight: AtomicUsize::new(0),
            cooldown: Mutex::new(None),
        })
    }

    fn snapshot(&self) -> Vec<Arc<Instance>> {
        self.instances.read().unwrap().clone()
    }

    fn stale(&self) -> bool {
        let now = self.created.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_refresh.load(Ordering::Relaxed))
            >= REFRESH_EVERY.as_millis() as u64
    }

    async fn resolve(&self) -> Result<Vec<SocketAddr>, HttpError> {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((self.hostname.as_str(), self.port))
            .await?
            .collect();
        if addrs.is_empty() {
            return Err(HttpError::Resolve(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "gateway name resolved to no addresses",
            )));
        }
        Ok(addrs)
    }

    /// Current instances, re-resolving when the list is empty or stale. Only one
    /// refresh runs at a time; others use the list already in place.
    async fn ready(&self) -> Result<Vec<Arc<Instance>>, HttpError> {
        let list = self.snapshot();
        if !list.is_empty() && !self.stale() {
            return Ok(list);
        }
        let _gate = self.refresh_gate.lock().await;
        // Another task may have refreshed while we waited for the gate.
        let list = self.snapshot();
        if !list.is_empty() && !self.stale() {
            return Ok(list);
        }
        let addrs = self.resolve().await?;
        let fresh = reconcile(&list, &addrs, |addr| self.make_instance(addr));
        *self.instances.write().unwrap() = fresh.clone();
        self.last_refresh
            .store(self.created.elapsed().as_millis() as u64, Ordering::Relaxed);
        Ok(fresh)
    }

    /// Sends the `build`-ed request to the least-busy instance, retrying on a
    /// different instance when one fails to connect (a connect error means no
    /// bytes were sent, so this never replays a put body to a server that saw it).
    /// Returns the response and a lease that keeps the instance counted as busy.
    async fn send(
        &self,
        build: &(dyn Fn(&reqwest::Client) -> reqwest::RequestBuilder + Send + Sync),
    ) -> Result<(Response, InFlight), HttpError> {
        let mut tried: Vec<SocketAddr> = Vec::new();
        loop {
            let list = self.ready().await?;
            let Some(instance) = select(&list, Instant::now(), &tried) else {
                return Err(HttpError::Resolve(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "no gateway instance could be reached",
                )));
            };
            instance.in_flight.fetch_add(1, Ordering::Relaxed);
            let lease = InFlight(instance.clone());
            match build(&instance.agent).send().await {
                Ok(res) => return Ok((res, lease)),
                Err(e) if e.is_connect() && tried.len() + 1 < MAX_CONNECT_ATTEMPTS => {
                    instance.mark_failed();
                    tried.push(instance.addr);
                    drop(lease);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// The transport behind a [`Client`].
enum Transport {
    /// One client with the rotating resolver, HTTP/1.1.
    Http1(reqwest::Client),
    /// A pool of per-instance HTTP/2 clients.
    Http2(Arc<Pool>),
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
    transport: Transport,
    put_url: Url,
    get_url: Url,
    capacity_url: Url,
    timeouts: Timeouts,
}

impl Client {
    /// Client with the default [`Options`] (HTTP/1.1 and the default [`Timeouts`]).
    ///
    /// # Panics
    /// Panics if the URL or authorization header is invalid, or HTTP client initialization fails.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_options(base_url, token, Options::default())
    }

    /// Client with custom timeouts and the default [`Protocol`].
    ///
    /// # Panics
    /// Panics if the URL or authorization header is invalid, or HTTP client initialization fails.
    pub fn with_timeouts(
        base_url: impl Into<String>,
        token: impl Into<String>,
        timeouts: Timeouts,
    ) -> Self {
        Self::with_options(
            base_url,
            token,
            Options {
                timeouts,
                ..Options::default()
            },
        )
    }

    /// Client with custom [`Options`], including the [`Protocol`].
    ///
    /// # Panics
    /// Panics if the URL or authorization header is invalid, or HTTP client initialization fails.
    pub fn with_options(
        base_url: impl Into<String>,
        token: impl Into<String>,
        options: Options,
    ) -> Self {
        let base_url = base_url.into();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", token.into()))
            .expect("invalid authorization header");
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);

        let put_url = Url::parse(&format!("{base_url}/v1/put")).expect("invalid gateway URL");
        let get_url = Url::parse(&format!("{base_url}/v1/get")).expect("invalid gateway URL");
        let capacity_url =
            Url::parse(&format!("{base_url}/v1/capacity")).expect("invalid gateway URL");

        let transport = match options.protocol {
            Protocol::Http1 => Transport::Http1(
                reqwest::Client::builder()
                    .default_headers(headers)
                    .connect_timeout(options.timeouts.connect)
                    .retry(reqwest::retry::never())
                    // A 307/308 would resend the put body and could store the blob twice.
                    .redirect(reqwest::redirect::Policy::none())
                    .dns_resolver(Arc::new(RotatingResolver::default()))
                    // HTTP/2 would multiplex every request over one connection, so one
                    // instance; use Protocol::Http2 for the per-instance pool instead.
                    .http1_only()
                    .build()
                    .expect("failed to build HTTP client"),
            ),
            Protocol::Http2 => {
                let host = put_url
                    .host_str()
                    .expect("gateway URL has no host")
                    .to_string();
                let port = put_url
                    .port_or_known_default()
                    .expect("gateway URL has no port");
                Transport::Http2(Arc::new(Pool::new(
                    host,
                    port,
                    headers,
                    options.timeouts.connect,
                )))
            }
        };

        Self {
            transport,
            put_url,
            get_url,
            capacity_url,
            timeouts: options.timeouts,
        }
    }

    /// Sends one request through the active transport. For HTTP/2 this picks the
    /// least-busy instance and returns a lease the caller holds until the response
    /// body is fully read, so the instance stays counted as busy for the whole
    /// request.
    async fn dispatch(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder + Send + Sync,
    ) -> Result<(Response, Option<InFlight>), HttpError> {
        match &self.transport {
            Transport::Http1(agent) => Ok((build(agent).send().await?, None)),
            Transport::Http2(pool) => {
                let (res, lease) = pool.send(&build).await?;
                Ok((res, Some(lease)))
            }
        }
    }

    /// Puts `data` and returns the receipt once its commitment is verified against `data`.
    pub async fn put(&self, data: Bytes) -> Result<Receipt, HttpError> {
        let (res, lease) = self
            .dispatch(|agent| {
                agent
                    .post(self.put_url.clone())
                    .timeout(self.timeouts.put)
                    .header("Content-Type", "application/octet-stream")
                    .body(data.clone())
            })
            .await?;
        let put: PutResponse = check(res).await?.json().await?;
        drop(lease);
        Ok(tokio::task::spawn_blocking(move || {
            verify(&data, &put.receipt, &put.commitment_proof)?;
            Ok::<_, VerifyError>(put.receipt)
        })
        .await??)
    }

    /// Gets the blob for `blob_id`.
    pub async fn get(&self, blob_id: &str) -> Result<Vec<u8>, HttpError> {
        let (res, _lease) = self
            .dispatch(|agent| {
                agent
                    .post(self.get_url.clone())
                    .timeout(self.timeouts.get)
                    .json(&serde_json::json!({ "blob_id": blob_id }))
            })
            .await?;
        let mut res = check(res).await?;
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
        let (res, _lease) = self
            .dispatch(|agent| {
                agent
                    .post(self.capacity_url.clone())
                    .timeout(CAPACITY_TIMEOUT)
                    .json(&serde_json::json!({"instances": instances, "minutes": minutes}))
            })
            .await?;
        Ok(check(res).await?.json().await?)
    }

    /// Returns the current reservation and fleet size.
    pub async fn capacity_status(&self) -> Result<CapacityStatus, HttpError> {
        let (res, _lease) = self
            .dispatch(|agent| {
                agent
                    .get(self.capacity_url.clone())
                    .timeout(CAPACITY_TIMEOUT)
            })
            .await?;
        Ok(check(res).await?.json().await?)
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

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 8443))
    }

    fn instance(n: u8, load: usize) -> Arc<Instance> {
        Arc::new(Instance {
            addr: addr(n),
            agent: reqwest::Client::new(),
            in_flight: AtomicUsize::new(load),
            cooldown: Mutex::new(None),
        })
    }

    #[test]
    fn reconcile_reuses_existing_and_drops_gone() {
        let existing = vec![instance(1, 5), instance(2, 0)];
        let made = AtomicUsize::new(0);
        // Resolve now returns 2 and 3: keep 2, add 3, drop 1.
        let fresh = reconcile(&existing, &[addr(2), addr(3)], |a| {
            made.fetch_add(1, Ordering::Relaxed);
            Arc::new(Instance {
                addr: a,
                agent: reqwest::Client::new(),
                in_flight: AtomicUsize::new(0),
                cooldown: Mutex::new(None),
            })
        });
        assert_eq!(
            made.load(Ordering::Relaxed),
            1,
            "only the new address is built"
        );
        // Instance 2 is the same Arc, so its live connection and counter survive.
        assert!(Arc::ptr_eq(&fresh[0], &existing[1]));
        assert_eq!(fresh[1].addr, addr(3));
        assert_eq!(fresh.len(), 2);
    }

    #[test]
    fn select_prefers_least_busy_then_falls_back_when_all_cooling() {
        let now = Instant::now();
        let soon = now + Duration::from_secs(10);
        let a = instance(1, 2);
        let b = instance(2, 1);
        let c = instance(3, 0);
        let list = vec![a.clone(), b.clone(), c.clone()];

        // Least in flight wins.
        assert_eq!(select(&list, now, &[]).unwrap().addr, addr(3));

        // A cooling instance is skipped even when it is the least busy.
        *c.cooldown.lock().unwrap() = Some(soon);
        assert_eq!(select(&list, now, &[]).unwrap().addr, addr(2));
        // Excluding the chosen one moves to the next least busy that is not cooling.
        assert_eq!(select(&list, now, &[addr(2)]).unwrap().addr, addr(1));

        // When every instance is cooling, fall back to the least busy overall.
        *a.cooldown.lock().unwrap() = Some(soon);
        *b.cooldown.lock().unwrap() = Some(soon);
        assert_eq!(select(&list, now, &[]).unwrap().addr, addr(3));

        // An expired cooldown counts as available again.
        *c.cooldown.lock().unwrap() = Some(now - Duration::from_secs(1));
        assert_eq!(select(&list, now, &[]).unwrap().addr, addr(3));

        assert!(select(&[], now, &[]).is_none());
    }
}
