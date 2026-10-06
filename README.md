# fibre-gateway-client

This library provides a client library for a Celestia Fibre gateway. 

A Fibre gateway provides easy access to a Celestia Fibre deployment by interfacing with validators on the client's behalf. 
The gateway accepts a raw payload, Reed-Solomon encodes it, and sends the resulting `blob` to Celestia for storage.
On success, the gatway returns a `blob_id` - a binding commitment to the posted data.

## Quick Start (Rust) 
Taken from [clients/rust/examples/quickstart.rs](../clients/rust/examples/quickstart.rs):

```toml
[dependencies]
fibre-gateway-client = { version = "0.1", features = ["http"] }
serde_json = "1"
```

```rust
use fibre_gateway_client::Client;

const GATEWAY: &str = "https://cf.celestia-corto.com:8443";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: quickstart <blob>")?;
    let data = std::fs::read(path)?;
    let client = Client::new(GATEWAY, std::env::var("TOKEN")?);

    // The Rust client's `put` method checks the commitment proof against `data` before it returns the receipt.
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

## Available Methods 

- **Put** sends one blob (32–128 MiB) to Celestia on your behalf and returns once it is available, with its `blob_id`.
- **Get** returns the blob by its `blob_id` for 24 h after the put.


## Docs

Start with the **[quick start](docs/client-guide.md#quick-start)** in the client guide. Then:

- [docs/client-guide.md](docs/client-guide.md): full reference (tokens, limits, retries, verification, client libraries).
- [docs/put-api.md](docs/put-api.md): API reference for `POST /v1/put`, `POST /v1/get` and `/v1/capacity`.
- [docs/openapi.yaml](docs/openapi.yaml): OpenAPI 3.1 spec for the same endpoints.
- [docs/contract.md](docs/contract.md): what the service promises.

Client libraries that verify the commitment:

- [clients/rust](clients/rust): crate `fibre-gateway-client`, with an optional HTTP client.
- [clients/python](clients/python/fibre_verify.py): one stdlib-only file.

Both are tested against [clients/testdata/commitment_vectors.json](clients/testdata/commitment_vectors.json).

## License

[Apache 2.0](LICENSE).
