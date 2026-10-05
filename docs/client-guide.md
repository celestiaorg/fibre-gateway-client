# Client guide

How to integrate the Fibre gateway: connecting, tokens, limits, errors, verification and client libraries.
New here? Start with [Getting started](getting-started.md).
The request and response formats are in the [API reference](put-api.md).
What we promise is in the [service contract](contract.md).

## Contents

- [Terms](#terms)
- [Connect](#connect)
- [Tokens](#tokens)
- [Blob sizes](#blob-sizes)
- [Limits](#limits)
- [Capacity](#capacity)
- [Timeouts](#timeouts)
- [Errors and retries](#errors-and-retries)
- [Retention and what to store](#retention-and-what-to-store)
- [Verification](#verification)
- [Client libraries](#client-libraries)
- [How verification works](#how-verification-works)

## Terms

| Term | Meaning |
|---|---|
| Blob | The bytes you put, 32 MiB − 5 to 128 MiB − 5 |
| Rows | The blob is laid out as 4096 equal rows: a 5-byte header, your data, then zero padding. The network adds 12,288 parity rows |
| Row root | Merkle root over all 16,384 rows |
| RLC | Random linear combination: one 16-byte value per row, with coefficients derived from the row root. It ties every row's content to the commitment |
| Commitment | `SHA256(row_root ‖ rlc_root)`, where `rlc_root` is the Merkle root of the per-row RLC values |
| `blob_id` | Version byte `00`, then the commitment. 33 bytes, 66 hex characters |
| Receipt | `chain_id`, `tx_hash`, `blob_id` and `promise_height` from the put response. Only `blob_id` is needed to read the blob |
| Commitment proof | Two row tree nodes (`row_root_siblings`) that let you recompute the row root from your own data |

## Connect

| Item | Value |
|---|---|
| Endpoint | `https://cf.celestia-corto.com:8443`. Port 8443 only |
| DNS | The name round-robins over the instances serving traffic |
| TLS | TLS 1.2 or newer. Public Let's Encrypt certificate for `cf.celestia-corto.com` and `*.cf.celestia-corto.com`. Standard verification, no custom CA |
| Chain | `corto-10` |

The same certificate and tokens work on every name.

HTTP client settings:

- **Keep-alive.** Reuse one client and its connection pool. Do not open a connection per request.
- **Spread load.** A pooled connection stays on one instance, and so does a single HTTP/2 connection.
  For high concurrency, open several connections.
- **No automatic replay.** Never let your HTTP library resend a put body on its own.
  Retry with a new request (see [Errors and retries](#errors-and-retries)).
  The Rust crate's `Client` never replays a `POST`.

## Tokens

Celestia issues one or more bearer tokens per client, out of band
(a password manager or another secure channel, never a ticket or chat).

- Send exactly one header, `Authorization: Bearer <token>`: the word `Bearer`, one space, the token.
- Every instance accepts the same tokens.
- **Rotation, no downtime:** we add your new token, you switch to it, then we remove the old one.
- **`401 unauthorized`** means the token is missing, wrong or retired, or the header was sent twice.
  Do not retry unchanged. If it starts after a rotation, switch to the new token.
- **Storage.** Never log the token or put it in a URL. Inject it at launch. Do not bake it into the image.

## Blob sizes

A put accepts 33,554,427 through 134,217,723 bytes (32 MiB − 5 through 128 MiB − 5).
Other sizes get `413` before the body is read.

Best sizes are `k × 262,144 − 5` bytes, for `k` from 128 to 512 (for example 134,217,723).
They fill the 4096 rows exactly, so no padding is wasted.

The gateway returns exactly the bytes you put. It never strips padding.
If your data is smaller than the minimum, pad it yourself and store the real length.

## Limits

| Limit | Value |
|---|---|
| Blob size | 33,554,427 to 134,217,723 bytes |
| Concurrent puts per instance | 48, then `429`. We recommend at most 32 |
| Concurrent gets per instance | 32, then `429` |
| Get request body | 4 KiB |
| Request headers | 16 KiB, sent within 5 s |
| Retention | 24 h after the put |
| Idempotency | None. A retried put may store a second copy |

A slot is held for the whole request. Above the limit the gateway returns `429` at once; it does not queue.
If you need more capacity, [reserve it](#capacity) or ask for more instances.

## Capacity

A sudden large job can see `429` until enough capacity is ready.
Before a large job, reserve a minimum number of instances for a while:

| Request | Meaning |
|---|---|
| `POST /v1/capacity` with `{"instances":20,"minutes":60}` | Keep at least `instances` (3–20) up for `minutes` (1–120) |
| `GET /v1/capacity` | Current reservation and fleet size |

Both return:

```json
{"floor":20,"expires_at":"2026-10-04T13:00:00+00:00","target":20,"active":8,"running":12,"eta_seconds":150}
```

| Field | Meaning |
|---|---|
| `floor` | Reserved instances. `0` when no reservation is active |
| `expires_at` | When the reservation ends, or `null` |
| `target` | Instances the fleet is scaling to |
| `active` | Instances serving traffic now |
| `running` | Instances started, including ones still warming up |
| `eta_seconds` | Estimated time until `target` instances serve traffic |

- Start the job after `eta_seconds`, or poll `GET` until `active` reaches `target`.
- There is one reservation for the whole fleet. A new `POST` replaces it; at most one change per 10 s.
- Only tokens allowed to reserve capacity may call it; others get `403 forbidden`. Ask us to allow yours.
- Capacity calls do not use put or get slots.

## Timeouts

| Operation | Gateway deadline | Client timeout to use |
|---|---|---|
| Connect | — | 10 s |
| Put | 150 s, plus 5 s to write the response | 160 s |
| Get | 120 s, plus 5 s to write the response | 130 s |

The put deadline covers receiving the body, upload, chain confirmation and the receipt lookup.
The body must arrive in full before the deadline.

The Rust crate's `Client` uses these timeouts by default. To change them:

```rust
use std::time::Duration;
use fibre_gateway_client::{Client, Timeouts};

let timeouts = Timeouts { put: Duration::from_secs(200), ..Timeouts::default() };
let client = Client::with_timeouts(GATEWAY, token, timeouts);
```

## Errors and retries

Every status code is listed in the [API reference](put-api.md#errors).

| Status | Retry? | How |
|---|---|---|
| `429`, `503` | Yes | Exponential backoff from 1 s, cap 30 s, full jitter, up to 10 attempts |
| `502`, `504` on get | Yes | Same backoff |
| `502`, `504` on put | Yes, with a new request | Start at 5 s, same cap. May store a second copy |
| Network error during a put | Yes, with a new request | Same as `502` on put |
| `400`, `401`, `404`, `413` | No | Fix the request or token first |

**Unknown outcome.** After a `502` or `504` on put, or a network error once the body started,
the blob may already be stored and paid for. A retry stores and pays again.
That is fine: keep the receipt you finally get.
`502 proof_unavailable` means the blob is stored and paid; report it to us.

## Retention and what to store

Blobs can be read for 24 h after the put. After that `/v1/get` returns `404`.

For each blob, store:

| What | Why |
|---|---|
| `blob_id` | `/v1/get` takes it as the request body |
| The time of the put | To know when the 24 h ends |
| Your real data length, if you padded | The gateway returns the padded bytes |

## Verification

**With our libraries you do not implement any verification.**

- **Rust `Client`:** `Client::put` checks the commitment before it returns. There is nothing else to do.
- **Rust with your own HTTP stack:** call `fibre_gateway_client::verify(&data, &put.receipt, &put.commitment_proof)`.
- **Python:** call `verify(data, put["blob_id"], put["commitment_proof"]["row_root_siblings"])` right after the put.

Each check fails if the gateway committed to bytes other than yours. After it passes, keep only the `blob_id`.
Reads need no check on your side: the gateway checks every piece it downloads against `blob_id`.

Checking the chain is optional, see below. To write your own verifier, see
[How verification works](#how-verification-works).

### On-chain check (optional)

The commitment check shows that `blob_id` names your data. It does not show that the chain accepted it.
Fetch the transaction from any validator RPC, `v0` through `v9`. They use HTTPS with a
Let's Encrypt certificate; plain `http://` is refused.

```sh
curl -s "https://v0.cf.celestia-corto.com:26657/tx?hash=0x<tx_hash>"
```

Check, in `result.tx_result`:

- `code` is absent or `0` (CometBFT omits a zero code).
- An event with `type` `celestia.fibre.v1.EventPayForFibre` has attribute `commitment` equal to
  `blob_id` without its first byte. The value is base64 inside JSON quotes: `"value":"\"q83v...=\""`.
- Optionally, its `signer` attribute is the gateway account you expect.

`result.tx` (base64 protobuf) holds the `MsgPayForFibre`. Its `payment_promise` has `commitment`,
`blob_version` (0), `namespace` and `blob_size` (4096 × `row_size`) if you want to check those too.

One RPC is one node's view. Query more than one, or use a light client, if that matters.
Blobstream will be the trust-minimized way to prove inclusion.

This Rust function, from [clients/rust/examples/put_verify_get.rs](../clients/rust/examples/put_verify_get.rs),
does the check. It needs `base64 = "0.22"` and `serde_json = "1"`.

```rust
use base64::{engine::general_purpose::STANDARD, Engine};

type Error = Box<dyn std::error::Error>;

/// Checks that a CometBFT `/tx` response succeeded and holds an
/// `EventPayForFibre` for `commitment`.
fn check_pay_for_fibre(tx: &serde_json::Value, commitment: &[u8]) -> Result<(), Error> {
    let result = &tx["result"]["tx_result"];
    // CometBFT omits `code` when it is 0.
    if result["code"].as_u64().unwrap_or(0) != 0 {
        return Err(format!("tx failed: {tx}").into());
    }
    let events = result["events"].as_array().ok_or("no events")?;
    for event in events {
        if event["type"] != "celestia.fibre.v1.EventPayForFibre" {
            continue;
        }
        for attr in event["attributes"].as_array().ok_or("no attributes")? {
            if attr["key"] != "commitment" {
                continue;
            }
            // The value is a JSON string holding base64.
            let value: String = serde_json::from_str(attr["value"].as_str().ok_or("bad value")?)?;
            if STANDARD.decode(value)? == commitment {
                return Ok(());
            }
        }
    }
    Err("no EventPayForFibre for this commitment".into())
}
```

Call it with `&parse_blob_id(&receipt.blob_id)?[1..]` as the commitment.
The example runs the whole flow: put, commitment check, on-chain check, get and hash check.

## Client libraries

| Language | Where | Commitment check | HTTP client |
|---|---|---|---|
| Rust | [clients/rust](../clients/rust), crate `fibre-gateway-client` | `verify` | `Client` (feature `http`) |
| Python | [clients/python/fibre_verify.py](../clients/python/fibre_verify.py) | `verify` | Use `requests` or similar |

Verifying a 128 MiB − 5 blob on one laptop core (Apple M5 Pro) takes about 0.6 s in Rust
and 3.7 s in pure Python. Use Rust in production.

### Rust

The crate is not on crates.io. Add it from the repository:

```toml
[dependencies]
fibre-gateway-client = { git = "https://github.com/celestiaorg/fibre-gateway-client", features = ["http"] }
```

- `Client::new(url, token)` builds a blocking client with the default [timeouts](#timeouts).
- `Client::put(&data)` uploads, verifies the commitment and returns the `Receipt`.
  It fails with `HttpError::Verify` if the proof does not match your data.
- `Client::get(blob_id)` returns the blob bytes.
- `Client::capacity(instances, minutes)` reserves [capacity](#capacity); `Client::capacity_status()` reads it.
- A non-`200` status is `HttpError::Request(e)` where `*e` is `ureq::Error::Status(code, response)`.
  `response.into_string()` gives the `{"error":"<code>"}` body.
  Network errors are `ureq::Error::Transport`.
- Without the `http` feature you get the types and `verify` only, for use with your own HTTP stack.

Examples, run from `clients/rust`:

```sh
TOKEN=<token> cargo run --release --features http --example quickstart -- blob.bin
GW=https://cf.celestia-corto.com:8443 TOKEN=<token> RPC=https://v0.cf.celestia-corto.com:26657 \
  cargo run --release --features http --example put_verify_get -- blob.bin
```

The crate needs `std`: `rsema1d` 1.2 pulls in rayon, memmap2, serde_json and reed-solomon-simd.
The `http` feature uses rustls with built-in Mozilla root certificates, so no system CA store is needed.
No `no_std` build is planned.

### Python

[clients/python/fibre_verify.py](../clients/python/fibre_verify.py) is one stdlib-only file.
`verify(data, blob_id, row_root_siblings)` raises `VerifyError` on a mismatch.
Run its tests with `python3 -m unittest` in `clients/python`.

A put and get with `requests` (`pip install requests`):

```python
import requests
from fibre_verify import verify  # clients/python/fibre_verify.py

BASE = "https://cf.celestia-corto.com:8443"
TOKEN = "<token>"
HEADERS = {"Authorization": f"Bearer {TOKEN}"}


def put(data: bytes) -> dict:
    assert 33_554_427 <= len(data) <= 134_217_723
    r = requests.post(
        f"{BASE}/v1/put", data=data, timeout=(10, 160),
        headers={**HEADERS, "Content-Type": "application/octet-stream"},
    )
    if r.status_code != 200:
        raise RuntimeError(f"put: HTTP {r.status_code}: {r.text.strip()}")
    put = r.json()
    # Raises VerifyError if the receipt does not commit to data.
    verify(data, put["blob_id"], put["commitment_proof"]["row_root_siblings"])
    return put


def get(blob_id: str) -> bytes:
    r = requests.post(
        f"{BASE}/v1/get", json={"blob_id": blob_id}, timeout=(10, 130),
        headers={**HEADERS, "Content-Type": "application/json"},
    )
    if r.status_code != 200:
        raise RuntimeError(f"get: HTTP {r.status_code}: {r.text.strip()}")
    return r.content


data = bytes(128 * 1024 * 1024 - 5)  # fill with your payload
blob_id = put(data)["blob_id"]  # keep only this
assert get(blob_id) == data
```

A network error after the put body started sending is an unknown outcome.
Retry it with a new request (see [Errors and retries](#errors-and-retries)).

## How verification works

You only need this to write your own verifier. The libraries above do all of it.

The gateway encodes your blob, so a faulty or dishonest gateway could commit to other bytes.
The commitment check recomputes the commitment from your own data and compares it with `blob_id`.
The Rust and Python verifiers are tested against the same vectors,
[clients/testdata/commitment_vectors.json](../clients/testdata/commitment_vectors.json).

### Why you must compute the RLC yourself

Never take RLC values, or an RLC root, from the gateway or anyone else.
Only an RLC you compute from your own rows ties the commitment to all of your data.

### Step by step

You only need this to write your own verifier. `verify` does exactly this.
Hashes: leaf = `SHA256(0x00 ‖ data)`, node = `SHA256(0x01 ‖ left ‖ right)`.

**Inclusion: rows to row root.**

1. **Row size.** `row_size = roundUp64(ceil((len + 5) / 4096))`, at least 64 bytes.
   For 128 MiB − 5 it is 32,768.
2. **Layout.** A 5-byte header (`0x00`, then `len` as big-endian `u32`), your data, then zeros,
   filling exactly 4096 rows in order.
3. **Root of your rows.** The Merkle root over the 4096 row leaves.
4. **Fold the siblings.** `row_root = node(node(root, s0), s1)`, with `s0` and `s1` the two
   `row_root_siblings`, lowest first. Your rows are the left-most quarter of the 16,384-row tree,
   so no Reed-Solomon encoding is needed.

**RLC: rows to commitment.**

1. **Coefficients.** `seed = SHA256(row_root ‖ le32(4096) ‖ le32(12288) ‖ le32(row_size))`, then
   `c_i = HashToGF128(SHA256(seed ‖ le32(i)))` for `i < row_size / 2`.
2. **RLC per row.** `rlc_j = Σ_i sym_i(row_j) · c_i` in GF(2^128), where `sym_i` are the row's
   16-bit Leopard symbols. The RLC is linear, so the 4096 original rows are enough.
3. **Compare.** Merkle root over the 16-byte `rlc_j` values gives `rlc_root`.
   Accept only if `SHA256(row_root ‖ rlc_root)` equals `blob_id` without its first byte.

The same steps written with lumina's
[`rsema1d`](https://github.com/celestiaorg/lumina/tree/main/rsema1d) 1.2 (add `rsema1d = "1.2"`
to your dependencies). This code runs as the `manual_steps_match_vectors` test in
[clients/rust/tests/vectors.rs](../clients/rust/tests/vectors.rs).

```rust
use fibre_gateway_client::{parse_blob_id, row_size, ORIGINAL_ROWS, PARITY_ROWS};
use rsema1d::codec::compute_rlc;
use rsema1d::crypto::{derive_coefficients, hash_internal, hash_leaf, sha256, MerkleTree};

fn manual_check(data: &[u8], blob_id: &str, siblings: [[u8; 32]; 2]) -> bool {
    // blob_id = version byte 0x00 ‖ 32-byte commitment.
    let id = parse_blob_id(blob_id).unwrap();
    assert_eq!(id[0], 0);

    // Lay out header ‖ data ‖ zero padding over K = 4096 rows of row_size bytes.
    let size = row_size(data.len());
    let mut flat = vec![0u8; ORIGINAL_ROWS * size];
    flat[1..5].copy_from_slice(&(data.len() as u32).to_be_bytes());
    flat[5..5 + data.len()].copy_from_slice(data);
    let rows: Vec<&[u8]> = flat.chunks(size).collect();

    // Row root: Merkle root of the K rows, then fold in the two siblings.
    let leaves = rows.iter().map(|row| hash_leaf(row)).collect();
    let mut row_root = MerkleTree::from_leaf_hashes(leaves).root();
    for sibling in &siblings {
        row_root = hash_internal(&row_root, sibling);
    }

    // RLC root: RLC of every original row, with coefficients derived from the row root.
    let coeffs = derive_coefficients(&row_root, ORIGINAL_ROWS, PARITY_ROWS, size);
    let rlc_leaves = rows
        .iter()
        .map(|row| hash_leaf(&compute_rlc(row, &coeffs).to_bytes()))
        .collect();
    let rlc_root = MerkleTree::from_leaf_hashes(rlc_leaves).root();

    // commitment = SHA256(row_root ‖ rlc_root).
    sha256(&[row_root, rlc_root].concat()) == id[1..]
}
```
