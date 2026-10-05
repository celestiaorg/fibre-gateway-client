# Getting started

The Fibre gateway stores one blob of 32–128 MiB on Celestia Fibre and reads it back.
A put returns a `blob_id` and a commitment proof.
The Rust and Python libraries check that proof for you. After that, the `blob_id` is all you need to read the blob.

This page takes you from zero to a verified put and get.
The [client guide](client-guide.md) is the full reference.

## 1. What you need

| Item | Value |
|---|---|
| Endpoint | `https://cf.celestia-corto.com:8443`. Standard TLS, publicly trusted certificate, no custom CA |
| Token | A bearer token from Celestia, sent out of band |
| Code | Read access to [celestiaorg/fibre-gateway-client](https://github.com/celestiaorg/fibre-gateway-client), for the Rust and Python libraries |
| A test blob | 33,554,427 to 134,217,723 bytes (32 MiB − 5 to 128 MiB − 5) |

The set of instances behind `cf.celestia-corto.com` changes. Always use the name, never pin IPs.

Send the token as `Authorization: Bearer <token>` on every request.
More in [Tokens](client-guide.md#tokens).

## 2. Try it with curl

```sh
GW=https://cf.celestia-corto.com:8443
TOKEN=<your token>

head -c 134217723 /dev/urandom > blob.bin   # 128 MiB − 5, the largest size

curl --fail-with-body --max-time 160 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @blob.bin "$GW/v1/put" -o put.json

curl --fail-with-body --max-time 130 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  --data "{\"blob_id\":\"$(jq -r .blob_id put.json)\"}" "$GW/v1/get" -o blob.out

cmp blob.bin blob.out && echo ok
```

The put takes a few seconds: it returns after the chain confirms the blob.
`put.json` looks like this:

```json
{"chain_id":"corto-10","tx_hash":"<64 hex>","blob_id":"<66 hex>","promise_height":123,
 "commitment_proof":{"row_root_siblings":["<64 hex>","<64 hex>"]}}
```

The get body needs only `blob_id`. Fields are described in the [API reference](put-api.md#put-response).
curl does not check the commitment proof. Use the Rust or Python library for that (next step).

## 3. Put, verify and get in Rust

The crate `fibre-gateway-client` is in [clients/rust](../clients/rust). It is not on crates.io.

```toml
[dependencies]
fibre-gateway-client = { git = "https://github.com/celestiaorg/fibre-gateway-client", features = ["http"] }
serde_json = "1"
```

This is [clients/rust/examples/quickstart.rs](../clients/rust/examples/quickstart.rs):

```rust
use fibre_gateway_client::Client;

const GATEWAY: &str = "https://cf.celestia-corto.com:8443";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: quickstart <blob>")?;
    let data = std::fs::read(path)?;
    let client = Client::new(GATEWAY, std::env::var("TOKEN")?);

    // `put` checks the commitment proof against `data` before it returns the receipt.
    let receipt = client.put(&data)?;
    // After the check, `blob_id` is all you need to keep.
    std::fs::write("blob_id.txt", &receipt.blob_id)?;

    let back = client.get(&receipt.blob_id)?;
    if back != data {
        return Err("read-back does not match".into());
    }
    println!("ok {} {}", receipt.blob_id, receipt.tx_hash);
    Ok(())
}
```

Run it from `clients/rust`:

```sh
TOKEN=<your token> cargo run --release --features http --example quickstart -- blob.bin
```

You do not need to verify anything yourself: `Client::put` checks the commitment against your data
before it returns, and fails with `HttpError::Verify` if it does not match.
In Python, call `verify` from `fibre_verify.py` once after the put ([Python](client-guide.md#python)).

The `Client` already uses the right timeouts (160 s put, 130 s get, 10 s connect) and never replays a put.
Error types are in [Client libraries: Rust](client-guide.md#rust).

## 4. Check the chain (optional)

The commitment check proves `blob_id` names your data. To also confirm the chain accepted the payment,
fetch `tx_hash` from a validator RPC and look for the `EventPayForFibre` with your commitment:

```sh
curl -s "https://v0.cf.celestia-corto.com:26657/tx?hash=0x<tx_hash>"
```

[clients/rust/examples/put_verify_get.rs](../clients/rust/examples/put_verify_get.rs) does the whole flow,
including this check. Details: [On-chain check](client-guide.md#on-chain-check-optional).

## 5. Store the blob ID

Keep, for each blob: the `blob_id`, the time of the put and, if you padded, the real length.
Blobs can be read for 24 h after the put.
See [Retention and what to store](client-guide.md#retention-and-what-to-store).

## 6. Pre-warm capacity for big jobs

Before a large job, reserve the instances you need so they are ready when it starts:

```sh
curl --fail-with-body --max-time 30 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"instances":20,"minutes":60}' "$GW/v1/capacity"
```

Start the job after `eta_seconds`. In Rust: `client.capacity(20, 60)?`.
Your token must be allowed to reserve capacity; ask us. Details: [Capacity](client-guide.md#capacity).

## 7. Before production

Read these sections of the client guide:

- [Errors and retries](client-guide.md#errors-and-retries): which errors to retry, and why a retried put may store a second copy.
- [Limits](client-guide.md#limits): sizes, concurrency per instance, retention.
- [Connect](client-guide.md#connect): keep-alive and spreading load over instances.
- [Blob sizes](client-guide.md#blob-sizes): sizes that waste no padding.

The [service contract](contract.md) lists what we promise and measured performance. There is no SLA yet.

Support: `<support contact>`.
