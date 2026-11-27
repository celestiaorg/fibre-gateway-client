//! Rate-limited Corto-1 writes through the gateway client. See docs/load.md.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fibre_gateway_client::{Client, HttpError, Options, Protocol, Receipt};
use serde::Serialize;
use serde_json::json;
use tokio::task::{JoinError, JoinSet};
use tokio::time::{sleep_until, Instant};

const DRAIN: Duration = Duration::from_secs(170);
const YEAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const USAGE: &str = "fibre-load --endpoint https://HOST:8443 --token-file PATH
  [--rate BLOBS_PER_SECOND] [--bytes BYTES] [--concurrency COUNT] [--duration SECONDS]
Defaults: --rate 1 --bytes 1048571 --concurrency 4 --duration 1800
Use --duration 0 for continuous load. Only corto-1 receipts are accepted.";

struct Config {
    endpoint: String,
    token_file: String,
    rate: f64,
    bytes: usize,
    concurrency: usize,
    duration: Duration,
}

impl Config {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let mut cfg = Self {
            endpoint: String::new(),
            token_file: String::new(),
            rate: 1.0,
            bytes: (1 << 20) - 5,
            concurrency: 4,
            duration: Duration::from_secs(1800),
        };
        let mut args = args;
        while let Some(flag) = args.next() {
            let value = args.next().ok_or("each flag needs a value")?;
            match flag.as_str() {
                "--endpoint" => cfg.endpoint = value,
                "--token-file" => cfg.token_file = value,
                "--rate" => cfg.rate = value.parse().map_err(|_| "invalid rate")?,
                "--bytes" => cfg.bytes = value.parse().map_err(|_| "invalid byte count")?,
                "--concurrency" => {
                    cfg.concurrency = value.parse().map_err(|_| "invalid concurrency")?
                }
                "--duration" => {
                    cfg.duration =
                        Duration::from_secs(value.parse().map_err(|_| "invalid duration")?)
                }
                _ => return Err("unknown flag"),
            }
        }
        let url = reqwest::Url::parse(&cfg.endpoint).map_err(|_| "invalid endpoint")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err("endpoint must be an HTTPS origin without credentials, query or fragment");
        }
        cfg.endpoint = url.as_str().trim_end_matches('/').to_owned();
        if cfg.token_file.is_empty() {
            return Err("--token-file is required");
        }
        if !cfg.rate.is_finite() || !(0.001..=10_000.0).contains(&cfg.rate) {
            return Err("rate must be between 0.001 and 10000 blobs/s");
        }
        if !((128 << 10) - 5..=(128 << 20) - 5).contains(&cfg.bytes) {
            return Err("bytes must be between 131067 and 134217723");
        }
        if !(1..=1024).contains(&cfg.concurrency) || cfg.duration > YEAR {
            return Err("concurrency must be 1..1024; duration must be 0..31536000 seconds");
        }
        Ok(cfg)
    }
}

#[derive(Default, Serialize)]
struct Stats {
    attempted: u64,
    attempted_bytes: u64,
    verified: u64,
    verified_bytes: u64,
    errors: u64,
    unknown: u64,
    skipped: u64,
    http_statuses: BTreeMap<u16, u64>,
    verified_latency_ms_sum: f64,
    verified_latency_ms_max: f64,
}

type Completion = (u64, Duration, Result<Receipt, HttpError>);

fn emit(out: &mut impl Write, value: serde_json::Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    writeln!(out)?;
    out.flush()
}

impl Stats {
    fn report(
        &self,
        out: &mut impl Write,
        kind: &str,
        start: Instant,
        admission: f64,
        reason: &str,
    ) -> io::Result<()> {
        let elapsed = start.elapsed().as_secs_f64();
        emit(
            out,
            json!({
                "type": kind, "elapsed_seconds": elapsed, "admission_seconds": admission,
                "reason": reason, "stats": self,
                "verified_bytes_per_second": self.verified_bytes as f64 / elapsed.max(f64::EPSILON),
                "verified_latency_ms_mean": self.verified_latency_ms_sum / self.verified.max(1) as f64,
            }),
        )
    }

    fn record(
        &mut self,
        result: Result<Completion, JoinError>,
        bytes: usize,
        out: &mut impl Write,
    ) -> io::Result<(Option<&'static str>, Option<Duration>)> {
        let (seq, elapsed, result) = match result {
            Ok(result) => result,
            Err(_) => {
                self.errors += 1;
                self.unknown += 1;
                return Ok((Some("worker_failed"), None));
            }
        };
        let latency = elapsed.as_secs_f64() * 1000.0;
        let mut fatal = None;
        let mut retry_after = None;
        let mut status = None;
        let outcome = match result {
            Ok(receipt) if receipt.chain_id == "corto-1" => {
                self.verified += 1;
                self.verified_bytes += bytes as u64;
                self.verified_latency_ms_sum += latency;
                self.verified_latency_ms_max = self.verified_latency_ms_max.max(latency);
                emit(
                    out,
                    json!({"type": "put", "seq": seq, "outcome": "verified", "bytes": bytes, "latency_ms": latency, "receipt": receipt}),
                )?;
                return Ok((None, None));
            }
            Ok(_) => {
                fatal = Some("wrong_chain");
                "wrong_chain"
            }
            Err(HttpError::Status(error)) => {
                status = Some(error.status);
                *self.http_statuses.entry(error.status).or_default() += 1;
                match error.status {
                    429 | 503 => {
                        retry_after = Some(error.retry_after.unwrap_or(Duration::ZERO));
                        "rejected"
                    }
                    500..=599 => {
                        self.unknown += 1;
                        retry_after = Some(Duration::ZERO);
                        "unknown"
                    }
                    _ => {
                        fatal = Some("http_rejected");
                        "rejected"
                    }
                }
            }
            Err(HttpError::Verify(_)) => {
                fatal = Some("invalid_proof");
                "invalid_proof"
            }
            Err(HttpError::Join(_)) => {
                self.unknown += 1;
                fatal = Some("verification_worker_failed");
                "unknown"
            }
            Err(HttpError::Resolve(_)) => {
                retry_after = Some(Duration::ZERO);
                "connect_failed"
            }
            Err(HttpError::Request(error)) => {
                retry_after = Some(Duration::ZERO);
                if error.is_connect() {
                    "connect_failed"
                } else {
                    self.unknown += 1;
                    "unknown"
                }
            }
        };
        self.errors += 1;
        // Error bodies and transport messages can contain credentials or user data.
        emit(
            out,
            json!({"type": "put", "seq": seq, "outcome": outcome, "status": status, "bytes": bytes, "latency_ms": latency}),
        )?;
        Ok((fatal, retry_after))
    }
}

fn payload(size: usize, run: u128, seq: u64) -> Bytes {
    let mut state = (run as u64) ^ seq.wrapping_mul(0x9e3779b97f4a7c15);
    let mut data = vec![0; size];
    for chunk in data.chunks_mut(8) {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    data[..16].copy_from_slice(&run.to_le_bytes());
    data[16..24].copy_from_slice(&seq.to_le_bytes());
    data.into()
}

// Move straight to the next future slot instead of sending a catch-up burst.
fn advance(next: &mut Instant, period: Duration, now: Instant) -> u64 {
    let late = now.duration_since(*next).as_nanos();
    let missed = late / period.as_nanos();
    let remainder = Duration::from_nanos((late % period.as_nanos()) as u64);
    *next = now + period - remainder;
    missed as u64
}

async fn run(
    cfg: &Config,
    client: Arc<Client>,
    stop: impl Future<Output = ()>,
    out: &mut impl Write,
) -> io::Result<(Stats, Option<&'static str>)> {
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / cfg.rate);
    let mut next = start;
    let mut report_at = start + Duration::from_secs(10);
    let end = start + cfg.duration;
    let mut cooling_until = start;
    let mut backoff = 1u64;
    let mut workers = JoinSet::new();
    let mut stats = Stats::default();
    let mut fatal = None;
    tokio::pin!(stop);
    emit(
        out,
        json!({"type": "start", "run_id": run_id.to_string(), "endpoint": cfg.endpoint, "chain_id": "corto-1", "rate": cfg.rate, "bytes": cfg.bytes, "concurrency": cfg.concurrency, "duration_seconds": cfg.duration.as_secs()}),
    )?;
    let reason = loop {
        tokio::select! {
            biased;
            _ = &mut stop => break "signal",
            _ = sleep_until(end), if !cfg.duration.is_zero() => break "duration",
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                let (failure, wait) = stats.record(result, cfg.bytes, out)?;
                if failure.is_some() {
                    fatal = failure;
                    break "fatal";
                }
                if let Some(wait) = wait {
                    let jitter = 0.5 + ((run_id as u64 ^ stats.attempted) % 1000) as f64 / 2000.0;
                    let wait = wait.max(Duration::from_secs_f64(backoff as f64 * jitter));
                    let now = Instant::now();
                    cooling_until = cooling_until.max(now.checked_add(wait).unwrap_or(now + YEAR));
                    backoff = (backoff * 2).min(30);
                } else {
                    backoff = 1;
                }
            }
            _ = sleep_until(report_at) => {
                stats.report(out, "progress", start, start.elapsed().as_secs_f64(), "running")?;
                report_at = Instant::now() + Duration::from_secs(10);
            }
            _ = sleep_until(next) => {
                let now = Instant::now();
                stats.skipped += advance(&mut next, period, now);
                if workers.len() >= cfg.concurrency || now < cooling_until {
                    stats.skipped += 1;
                    continue;
                }
                stats.attempted += 1;
                stats.attempted_bytes += cfg.bytes as u64;
                let seq = stats.attempted;
                let data = payload(cfg.bytes, run_id, seq);
                let client = Arc::clone(&client);
                workers.spawn(async move {
                    let start = Instant::now();
                    let result = client.put(data).await;
                    (seq, start.elapsed(), result)
                });
            }
        }
    };
    let admission = start.elapsed().as_secs_f64();
    let drain_end = Instant::now() + DRAIN;
    while !workers.is_empty() {
        tokio::select! {
            biased;
            result = workers.join_next() => {
                let (failure, _) = stats.record(result.unwrap(), cfg.bytes, out)?;
                fatal = fatal.or(failure);
            }
            _ = sleep_until(drain_end) => {
                stats.errors += workers.len() as u64;
                stats.unknown += workers.len() as u64;
                workers.abort_all();
                fatal = fatal.or(Some("drain_timeout"));
                break;
            }
        }
    }
    stats.report(out, "summary", start, admission, fatal.unwrap_or(reason))?;
    Ok((stats, fatal))
}

async fn execute(cfg: &Config, token: String) -> io::Result<(Stats, Option<&'static str>)> {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let stop = async {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };
    let client = Arc::new(Client::with_options(
        &cfg.endpoint,
        token,
        Options {
            protocol: Protocol::Http2,
            ..Options::default()
        },
    ));
    run(cfg, client, stop, &mut io::stdout()).await
}

fn main() {
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return;
    }
    let cfg = match Config::parse(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let token = match std::fs::read_to_string(&cfg.token_file) {
        Ok(token) => token,
        Err(error) => {
            eprintln!("cannot read token file: {}", error.kind());
            std::process::exit(2);
        }
    };
    let token = token.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        eprintln!("token file must contain one non-empty bearer token");
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not start runtime");
    let result = runtime.block_on(execute(&cfg, token.to_owned()));
    // SDK verification uses blocking tasks, which cannot be aborted mid-verification.
    runtime.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok((_, None)) => {}
        Ok((_, Some(reason))) => {
            eprintln!("load stopped: {reason}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("load stopped: signal setup or output failed");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::thread;

    fn config(extra: &[&str]) -> Config {
        let mut args = vec![
            "--endpoint",
            "https://gateway.example:8443",
            "--token-file",
            "token",
        ];
        args.extend_from_slice(extra);
        Config::parse(args.into_iter().map(String::from)).unwrap()
    }

    #[test]
    fn rejects_invalid_configuration() {
        for pair in [
            ["--rate", "NaN"],
            ["--rate", "inf"],
            ["--rate", "0"],
            ["--rate", "1000000000"],
            ["--bytes", "131066"],
            ["--bytes", "134217724"],
            ["--concurrency", "0"],
            ["--duration", "18446744073709551615"],
            ["--endpoint", "http://gateway.example"],
            ["--endpoint", "https://secret@gateway.example"],
            ["--endpoint", "https://gateway.example/?token=secret"],
        ] {
            let args = [
                "--endpoint",
                "https://gateway.example",
                "--token-file",
                "token",
                pair[0],
                pair[1],
            ];
            assert!(
                Config::parse(args.into_iter().map(String::from)).is_err(),
                "{pair:?}"
            );
        }
        assert_eq!(config(&["--duration", "0"]).duration, Duration::ZERO);
    }

    #[tokio::test]
    async fn canonical_endpoint_is_safe_for_the_client() {
        let cfg = config(&["--endpoint", "https://gateway.example:8443 "]);
        assert_eq!(cfg.endpoint, "https://gateway.example:8443");
        Client::new(cfg.endpoint, "token");
    }

    #[tokio::test(start_paused = true)]
    async fn pacing_skips_late_slots_without_a_burst() {
        let mut next = Instant::now();
        let period = Duration::from_millis(100);
        assert_eq!(advance(&mut next, period, Instant::now()), 0);
        tokio::time::advance(Duration::from_millis(350)).await;
        assert_eq!(advance(&mut next, period, Instant::now()), 2);
        assert_eq!(next - Instant::now(), Duration::from_millis(50));
    }

    #[tokio::test(start_paused = true)]
    async fn timer_delay_does_not_accumulate_rate_drift() {
        let start = Instant::now();
        let period = Duration::from_millis(100);
        let mut next = start + period;
        for tick in 1..=10 {
            let wake = start + Duration::from_millis(tick * 100 + 10);
            assert_eq!(advance(&mut next, period, wake), 0);
            assert_eq!(next, start + Duration::from_millis((tick + 1) * 100));
        }
    }

    #[test]
    fn payloads_differ_between_requests_and_runs() {
        let first = payload(1024, 10, 1);
        assert_ne!(first, payload(1024, 10, 2));
        assert_ne!(first, payload(1024, 11, 1));
        assert!(first.iter().filter(|&&b| b != 0).count() > 900);
    }

    fn server(
        count: usize,
        status: &'static str,
        delay: Duration,
    ) -> (Arc<Client>, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Arc::new(Client::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "token",
        ));
        let server = thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                handlers.push(thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    assert!(line.starts_with("POST /v1/put "));
                    let mut length = 0;
                    let mut authorised = false;
                    loop {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" { break; }
                        let (name, value) = line.split_once(':').unwrap();
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                        if name.eq_ignore_ascii_case("authorization") {
                            authorised = value.trim() == "Bearer token";
                        }
                    }
                    assert!(authorised);
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    thread::sleep(delay);
                    write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 2\r\nRetry-After: 1\r\nConnection: close\r\n\r\n{{}}").unwrap();
                    body
                }));
            }
            handlers.into_iter().map(|h| h.join().unwrap()).collect()
        });
        (client, server)
    }

    #[tokio::test]
    async fn stops_admission_on_signal() {
        let cfg = config(&[]);
        let client = Arc::new(Client::new("http://127.0.0.1:1", "token"));
        let (stats, fatal) = run(&cfg, client, std::future::ready(()), &mut Vec::new())
            .await
            .unwrap();
        assert_eq!(stats.attempted, 0);
        assert!(fatal.is_none());
    }

    #[tokio::test]
    async fn bounds_concurrency_and_drains_unknown_puts_without_replay() {
        let mut cfg = config(&["--rate", "100", "--concurrency", "2"]);
        cfg.duration = Duration::from_millis(80);
        let (client, server) = server(2, "502 Bad Gateway", Duration::from_millis(150));
        let mut out = Vec::new();
        let (stats, fatal) = run(&cfg, client, std::future::pending(), &mut out)
            .await
            .unwrap();
        let bodies = server.join().unwrap();
        assert_eq!(stats.attempted, 2);
        assert_eq!(stats.unknown, 2);
        assert_eq!(stats.errors, 2);
        assert_eq!(stats.verified, 0);
        assert!(stats.skipped > 0);
        assert_ne!(bodies[0], bodies[1]);
        assert!(fatal.is_none());
        let output = String::from_utf8(out).unwrap();
        let final_line: serde_json::Value =
            serde_json::from_str(output.lines().last().unwrap()).unwrap();
        assert_eq!(final_line["type"], "summary");
        assert_eq!(final_line["stats"]["unknown"], 2);
    }

    #[tokio::test]
    async fn honours_retry_after_and_stops_on_bad_credentials() {
        for (status, fatal_expected) in [
            ("429 Too Many Requests", None),
            ("503 Service Unavailable", None),
            ("401 Unauthorized", Some("http_rejected")),
        ] {
            let mut cfg = config(&["--rate", "10"]);
            cfg.duration = Duration::from_millis(250);
            let (client, server) = server(1, status, Duration::ZERO);
            let (stats, fatal) = run(&cfg, client, std::future::pending(), &mut Vec::new())
                .await
                .unwrap();
            server.join().unwrap();
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.errors, 1);
            assert_eq!(stats.unknown, 0);
            assert_eq!(fatal, fatal_expected);
        }
    }

    #[test]
    fn only_verified_corto_receipts_count_as_success() {
        let mut stats = Stats::default();
        let receipt = Receipt {
            chain_id: "other-chain".into(),
            tx_hash: "ab".repeat(32),
            blob_id: "00".repeat(33),
            promise_height: 1,
        };
        let (fatal, _) = stats
            .record(
                Ok((1, Duration::from_secs(1), Ok(receipt.clone()))),
                1024,
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(fatal, Some("wrong_chain"));
        assert_eq!(stats.verified, 0);
        let receipt = Receipt {
            chain_id: "corto-1".into(),
            ..receipt
        };
        stats
            .record(
                Ok((2, Duration::from_millis(50), Ok(receipt))),
                1024,
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(stats.verified_bytes, 1024);
        assert_eq!(stats.verified_latency_ms_sum, 50.0);
        let result = Err(HttpError::Verify(
            fibre_gateway_client::VerifyError::Mismatch,
        ));
        let (fatal, _) = stats
            .record(Ok((3, Duration::ZERO, result)), 1024, &mut Vec::new())
            .unwrap();
        assert_eq!(fatal, Some("invalid_proof"));
        assert_eq!(stats.verified, 1);
    }
}
