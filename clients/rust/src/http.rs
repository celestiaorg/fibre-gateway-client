//! Async client for `/v1/put`, `/v1/get` and `/v1/capacity`, requiring Tokio.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Resolving the gateway name to instance addresses failed (HTTP/2 only).
    #[error("resolving the gateway failed: {0}")]
    Resolve(std::io::Error),
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
    /// HTTP/1.1 with a resolver that rotates the instance addresses: one
    /// connection per in-flight request, each landing on a different instance. The default; it spreads
    /// load without an upload penalty.
    #[default]
    Http1,
    /// HTTP/2 with one multiplexed connection per instance. Resolves the fleet,
    /// keeps a connection to every instance, sends requests to them in turn and
    /// re-resolves about once a minute. Large uploads can be slower than
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
    /// Skip this instance until this instant after a connect failure.
    cooldown: Mutex<Option<Instant>>,
}

impl Instance {
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

/// The next instance round-robin from `start` that is not in cooldown, or the
/// next one at all when every instance is cooling. `exclude` holds addresses
/// already tried this request.
fn select(
    instances: &[Arc<Instance>],
    start: usize,
    now: Instant,
    exclude: &[SocketAddr],
) -> Option<Arc<Instance>> {
    let n = instances.len();
    let pick = |allow_cooling: bool| {
        (0..n)
            .map(|k| &instances[(start + k) % n])
            .find(|i| !exclude.contains(&i.addr) && (allow_cooling || !i.cooling(now)))
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
    state: Mutex<State>,
    /// Round-robin position.
    next: AtomicUsize,
}

/// The instance list and when it was resolved, behind one lock so checking for
/// staleness and claiming the refresh is a single step.
#[derive(Default)]
struct State {
    instances: Vec<Arc<Instance>>,
    /// When the list was last resolved, or tried to be.
    refreshed_at: Option<Instant>,
}

impl State {
    fn fresh(&self) -> bool {
        !self.instances.is_empty()
            && self
                .refreshed_at
                .is_some_and(|at| at.elapsed() < REFRESH_EVERY)
    }
}

impl Pool {
    fn new(hostname: String, port: u16, headers: HeaderMap, connect_timeout: Duration) -> Self {
        Self {
            hostname,
            port,
            headers,
            connect_timeout,
            state: Mutex::new(State::default()),
            next: AtomicUsize::new(0),
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
            cooldown: Mutex::new(None),
        })
    }

    async fn resolve(&self) -> Result<Vec<SocketAddr>, HttpError> {
        let lookup = tokio::net::lookup_host((self.hostname.as_str(), self.port));
        // The system resolver has no deadline of its own.
        let addrs: Vec<SocketAddr> = tokio::time::timeout(self.connect_timeout, lookup)
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "resolving timed out",
                ))
            })
            .map_err(HttpError::Resolve)?
            .collect();
        if addrs.is_empty() {
            return Err(HttpError::Resolve(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "gateway name resolved to no addresses",
            )));
        }
        Ok(addrs)
    }

    /// The next instance to use (see [`select`]), re-resolving the fleet first
    /// when the list is stale or empty. Only the caller that finds the list stale
    /// refreshes it; the others keep using the current list meanwhile. A failed
    /// refresh keeps a non-empty list, so only an empty one fails with the lookup
    /// error.
    async fn pick(&self, exclude: &[SocketAddr]) -> Result<Option<Arc<Instance>>, HttpError> {
        {
            let mut state = self.state.lock().unwrap();
            if state.fresh() {
                return Ok(self.select(&state.instances, exclude));
            }
            // Claim the refresh: concurrent callers keep using the current list,
            // and a failing resolver is not hit on every request.
            state.refreshed_at = Some(Instant::now());
        }
        let resolved = self.resolve().await;
        let mut state = self.state.lock().unwrap();
        match resolved {
            Ok(addrs) => {
                // Reconcile against the list as it is now, not as it was before
                // the lookup, so a concurrent refresh never duplicates instances.
                let fresh = reconcile(&state.instances, &addrs, |a| self.make_instance(a));
                state.instances = fresh;
            }
            Err(e) if state.instances.is_empty() => return Err(e),
            Err(_) => {}
        }
        Ok(self.select(&state.instances, exclude))
    }

    fn select(&self, instances: &[Arc<Instance>], exclude: &[SocketAddr]) -> Option<Arc<Instance>> {
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        select(instances, start, Instant::now(), exclude)
    }

    /// Sends the `build`-ed request to the next instance, retrying on a different
    /// one when it fails to connect (a connect error means no bytes were sent, so
    /// this never replays a put body to a server that saw it). When every attempt
    /// fails to connect, returns the last connect error.
    async fn send(
        &self,
        build: &(dyn Fn(&reqwest::Client) -> reqwest::RequestBuilder + Send + Sync),
    ) -> Result<Response, HttpError> {
        let mut tried: Vec<SocketAddr> = Vec::new();
        let mut last_err = None;
        loop {
            let Some(instance) = self.pick(&tried).await? else {
                // Every instance was tried and failed to connect.
                return Err(match last_err {
                    Some(e) => HttpError::Request(e),
                    None => HttpError::Resolve(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "no gateway instance to connect to",
                    )),
                });
            };
            match build(&instance.agent).send().await {
                Ok(res) => return Ok(res),
                Err(e) if e.is_connect() => {
                    instance.mark_failed();
                    tried.push(instance.addr);
                    if tried.len() >= MAX_CONNECT_ATTEMPTS {
                        return Err(e.into());
                    }
                    last_err = Some(e);
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

    /// Sends one request through the active transport.
    async fn dispatch(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder + Send + Sync,
    ) -> Result<Response, HttpError> {
        match &self.transport {
            Transport::Http1(agent) => Ok(build(agent).send().await?),
            Transport::Http2(pool) => pool.send(&build).await,
        }
    }

    /// Puts `data` and returns the receipt once its commitment is verified against `data`.
    pub async fn put(&self, data: Bytes) -> Result<Receipt, HttpError> {
        let res = self
            .dispatch(|agent| {
                agent
                    .post(self.put_url.clone())
                    .timeout(self.timeouts.put)
                    .header("Content-Type", "application/octet-stream")
                    .body(data.clone())
            })
            .await?;
        let put: PutResponse = check(res).await?.json().await?;
        Ok(tokio::task::spawn_blocking(move || {
            verify(&data, &put.receipt, &put.commitment_proof)?;
            Ok::<_, VerifyError>(put.receipt)
        })
        .await??)
    }

    /// Gets the blob for `blob_id`.
    pub async fn get(&self, blob_id: &str) -> Result<Vec<u8>, HttpError> {
        let res = self
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

    /// Reserves at least `instances` gateway instances (3..=20) for `minutes` (1..=1440, i.e. up to 24 h).
    /// The token must be allowed to reserve capacity.
    pub async fn capacity(
        &self,
        instances: u32,
        minutes: u32,
    ) -> Result<CapacityStatus, HttpError> {
        let res = self
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
        let res = self
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

    fn instance(n: u8) -> Arc<Instance> {
        Arc::new(Instance {
            addr: addr(n),
            agent: reqwest::Client::new(),
            cooldown: Mutex::new(None),
        })
    }

    /// Serves `/v1/capacity` over cleartext HTTP/2 on 127.0.0.1 and counts requests.
    async fn h2_server() -> (u16, Arc<AtomicUsize>) {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |_req| {
                        counter.fetch_add(1, Ordering::Relaxed);
                        let body = r#"{"floor":0,"expires_at":null,"target":3,"active":3,"running":3,"eta_seconds":0}"#;
                        async move {
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(
                                http_body_util::Full::new(Bytes::from(body)),
                            ))
                        }
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (port, hits)
    }

    fn http2_client(url: String) -> Client {
        let options = Options {
            protocol: Protocol::Http2,
            ..Default::default()
        };
        Client::with_options(url, "token", options)
    }

    fn pool(client: &Client) -> &Pool {
        match &client.transport {
            Transport::Http2(pool) => pool,
            Transport::Http1(_) => panic!("not HTTP/2"),
        }
    }

    #[tokio::test]
    async fn http2_pool_serves_concurrent_requests() {
        let (port, hits) = h2_server().await;
        // localhost may also resolve to ::1, where nothing listens: that instance fails to
        // connect, and the request moves to 127.0.0.1.
        let client = Arc::new(http2_client(format!("http://localhost:{port}")));
        let requests: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move { client.capacity_status().await })
            })
            .collect();
        for r in requests {
            assert_eq!(r.await.unwrap().unwrap().active, 3);
        }
        assert_eq!(hits.load(Ordering::Relaxed), 8);
        let list = pool(&client).state.lock().unwrap().instances.clone();
        assert!(list
            .iter()
            .any(|i| i.addr.ip().is_loopback() && i.addr.is_ipv4()));
        // The first requests all found the list empty and resolved; each reconciled
        // against the list as it was then, so no address got two instances.
        let mut addrs: Vec<_> = list.iter().map(|i| i.addr).collect();
        addrs.sort();
        addrs.dedup();
        assert_eq!(addrs.len(), list.len());
    }

    #[tokio::test]
    async fn http2_pool_returns_the_connect_error_when_every_instance_fails() {
        // A port nothing listens on, on a name with a single address: one instance.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let client = http2_client(format!("http://127.0.0.1:{port}"));
        match client.capacity_status().await {
            Err(HttpError::Request(e)) => assert!(e.is_connect(), "{e}"),
            other => panic!("expected the connect error, got {other:?}"),
        }
        let list = pool(&client).state.lock().unwrap().instances.clone();
        assert_eq!(list.len(), 1);
        assert!(list[0].cooling(Instant::now()));
    }

    #[tokio::test]
    async fn http2_pool_keeps_its_list_when_a_refresh_fails() {
        let pool = Pool::new(
            "gateway.invalid".into(),
            8443,
            HeaderMap::new(),
            Duration::from_secs(2),
        );
        pool.state.lock().unwrap().instances = vec![instance(1)];
        assert!(!pool.state.lock().unwrap().fresh());
        let picked = pool.pick(&[]).await.unwrap().unwrap();
        assert_eq!(picked.addr, addr(1));
        let state = pool.state.lock().unwrap();
        assert_eq!(state.instances.len(), 1);
        assert!(
            state.fresh(),
            "the failed attempt counts, so it is not retried at once"
        );
    }

    #[test]
    fn reconcile_reuses_existing_and_drops_gone() {
        let existing = vec![instance(1), instance(2)];
        let made = AtomicUsize::new(0);
        // Resolve now returns 2 and 3: keep 2, add 3, drop 1.
        let fresh = reconcile(&existing, &[addr(2), addr(3)], |a| {
            made.fetch_add(1, Ordering::Relaxed);
            Arc::new(Instance {
                addr: a,
                agent: reqwest::Client::new(),
                cooldown: Mutex::new(None),
            })
        });
        assert_eq!(
            made.load(Ordering::Relaxed),
            1,
            "only the new address is built"
        );
        // Instance 2 is the same Arc, so its live connection survives.
        assert!(Arc::ptr_eq(&fresh[0], &existing[1]));
        assert_eq!(fresh[1].addr, addr(3));
        assert_eq!(fresh.len(), 2);
    }

    #[test]
    fn select_goes_round_robin_and_skips_cooling_then_falls_back() {
        let now = Instant::now();
        let soon = now + Duration::from_secs(10);
        let list = vec![instance(1), instance(2), instance(3)];
        let pick = |start, exclude: &[SocketAddr]| select(&list, start, now, exclude).unwrap().addr;

        // Round-robin from `start`, wrapping around.
        assert_eq!(
            [0, 1, 2, 3].map(|n| pick(n, &[])),
            [addr(1), addr(2), addr(3), addr(1)]
        );

        // A cooling instance is skipped, and so is one already tried.
        *list[0].cooldown.lock().unwrap() = Some(soon);
        assert_eq!(pick(0, &[]), addr(2));
        assert_eq!(pick(0, &[addr(2)]), addr(3));

        // When every instance is cooling, take the next one anyway.
        for i in &list {
            *i.cooldown.lock().unwrap() = Some(soon);
        }
        assert_eq!(pick(1, &[]), addr(2));

        // An expired cooldown counts as available again.
        *list[2].cooldown.lock().unwrap() = Some(now - Duration::from_secs(1));
        assert_eq!(pick(0, &[]), addr(3));

        assert!(select(&[], 0, now, &[]).is_none());
    }
}
