# fibre-gateway-client

Client libraries and docs for the Fibre gateway, an HTTPS API that stores blobs on Celestia Fibre and reads them back.

## What put and get do

[Celestia](https://celestia.org) is a data availability network: it guarantees that published data can be downloaded by anyone who asks.
Fibre is its high-throughput path for large blobs. The gateway runs the Fibre protocol for you,
so you only make HTTPS calls. You need no Celestia account, keys or TIA; the gateway pays for every put.

- **Put** sends one blob, 32–128 MiB. The gateway splits it into pieces, adds redundancy
  (erasure coding) and spreads the pieces over Celestia's validators. Each validator checks its pieces and signs.
  The gateway then records the payment and the blob's commitment on the Celestia chain.
- **A successful put (`200`) means your data is available.** Validators holding at least 2/3 of the stake
  have signed that they store their pieces, and the chain has accepted the payment.
  The blob can be read back for 24 h, even if some validators go offline.
- **The `blob_id`** in the response is a fingerprint of your data (a cryptographic commitment).
  Our libraries recompute it from your own bytes, so you do not have to trust the gateway that it stored your data and not something else.
- **Get** takes a `blob_id`. The gateway fetches enough pieces from the validators, checks each one against the `blob_id`,
  rebuilds the blob and returns exactly the bytes you put.

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

`spec.go` only exposes the spec and the vectors to Go tooling. There is no Go client.

## License

[Apache 2.0](LICENSE).
