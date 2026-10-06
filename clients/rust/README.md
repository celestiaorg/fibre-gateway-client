# fibre-gateway-client

Rust client for the Fibre gateway: put a blob, get it back by its `blob_id`.

```toml
[dependencies]
fibre-gateway-client = { version = "0.1", features = ["http"] }
```

```rust
use fibre_gateway_client::Client;

let client = Client::new("https://cf.celestia-corto.com:8443", token);

// `put` checks the commitment against `data` before it returns.
// There is no verification left for you to do.
let receipt = client.put(&data)?;

// Keep only the blob_id.
let back = client.get(&receipt.blob_id)?;
```

- Feature `http` adds the blocking `Client` (put, get, capacity). It uses rustls with built-in root certificates.
- Without it you get the receipt types and `verify`, for use with your own HTTP stack.
  Call `verify(&data, &put.receipt, &put.commitment_proof)` after every put.

Blobs are 32 MiB − 5 to 128 MiB − 5 bytes and can be read for 24 h after the put.
You need a bearer token from Celestia.

License: Apache-2.0.
