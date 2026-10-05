//! Runs the blocking client against a one-shot local server.
#![cfg(feature = "http")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use fibre_gateway_client::{CapacityStatus, Client, HttpError, Timeouts, VerifyError};

/// A request's target and body.
type Seen = Vec<(String, Vec<u8>)>;

/// Serves one request per response and returns each request's target and body.
fn serve(responses: Vec<String>) -> (String, thread::JoinHandle<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let target = line.split(' ').nth(1).unwrap().to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            seen.push((target, body));
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
        }
        seen
    });
    (url, handle)
}

fn vector() -> serde_json::Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../testdata/commitment_vectors.json"
    );
    let file: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    file["vectors"][1].clone()
}

fn data(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| ((i as u32).wrapping_mul(2654435761) >> 24) as u8)
        .collect()
}

fn put_response(v: &serde_json::Value) -> String {
    serde_json::json!({
        "chain_id": "chain",
        "tx_hash": "ab".repeat(32),
        "blob_id": v["blob_id"],
        "promise_height": 9,
        "commitment_proof": {"row_root_siblings": v["row_root_siblings"]},
    })
    .to_string()
}

#[test]
fn put_verifies_and_get_sends_blob_id() {
    let v = vector();
    let data = data(v["size"].as_u64().unwrap() as usize);
    let (url, server) = serve(vec![put_response(&v), "xxx".into()]);
    let client = Client::new(url, "token");

    let receipt = client.put(&data).unwrap();
    assert_eq!(client.get(&receipt.blob_id).unwrap(), b"xxx");

    let seen = server.join().unwrap();
    assert_eq!(seen[0].0, "/v1/put");
    assert_eq!(seen[0].1, data);
    assert_eq!(seen[1].0, "/v1/get");
    let sent: serde_json::Value = serde_json::from_slice(&seen[1].1).unwrap();
    assert_eq!(sent.as_object().unwrap().len(), 1);
    assert_eq!(sent["blob_id"], v["blob_id"]);
}

#[test]
fn put_rejects_a_receipt_for_other_data() {
    let v = vector();
    let mut data = data(v["size"].as_u64().unwrap() as usize);
    data[0] ^= 1;
    let (url, server) = serve(vec![put_response(&v)]);
    let err = Client::new(url, "token").put(&data).unwrap_err();
    assert!(
        matches!(err, HttpError::Verify(VerifyError::Mismatch)),
        "{err}"
    );
    server.join().unwrap();
}

/// Accepts one connection, writes `head` and then stalls for 10 s.
fn stall(head: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        thread::sleep(Duration::from_secs(10));
    });
    url
}

fn short_timeouts() -> Timeouts {
    Timeouts {
        put: Duration::from_millis(300),
        get: Duration::from_millis(300),
        ..Timeouts::default()
    }
}

#[test]
fn default_timeouts_outlast_the_gateway() {
    let t = Timeouts::default();
    assert_eq!(t.connect, Duration::from_secs(10));
    assert_eq!(t.put, Duration::from_secs(160));
    assert_eq!(t.get, Duration::from_secs(130));
}

#[test]
fn put_times_out_on_a_silent_server() {
    let client = Client::with_timeouts(stall(""), "token", short_timeouts());
    let start = Instant::now();
    let err = client.put(b"data").unwrap_err();
    assert!(matches!(err, HttpError::Request(_)), "{err}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn get_times_out_while_reading_the_body() {
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
    let client = Client::with_timeouts(stall(head), "token", short_timeouts());
    let start = Instant::now();
    let err = client.get("00").unwrap_err();
    assert!(matches!(err, HttpError::Io(_)), "{err}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn capacity_reserves_and_reads_status() {
    let reserved = r#"{"floor":5,"expires_at":"2026-10-04T12:00:00+00:00","target":5,"active":3,"running":4,"eta_seconds":90}"#;
    let idle = r#"{"floor":0,"expires_at":null,"target":3,"active":3,"running":3,"eta_seconds":0}"#;
    let (url, server) = serve(vec![reserved.into(), idle.into()]);
    let client = Client::new(url, "token");

    let status = client.capacity(5, 30).unwrap();
    assert_eq!(
        status,
        CapacityStatus {
            floor: 5,
            expires_at: Some("2026-10-04T12:00:00+00:00".into()),
            target: 5,
            active: 3,
            running: 4,
            eta_seconds: 90,
        }
    );
    let status = client.capacity_status().unwrap();
    assert_eq!((status.floor, status.expires_at), (0, None));

    let seen = server.join().unwrap();
    assert_eq!(seen[0].0, "/v1/capacity");
    let sent: serde_json::Value = serde_json::from_slice(&seen[0].1).unwrap();
    assert_eq!(sent, serde_json::json!({"instances": 5, "minutes": 30}));
    assert_eq!(seen[1].0, "/v1/capacity");
    assert!(seen[1].1.is_empty());
}
