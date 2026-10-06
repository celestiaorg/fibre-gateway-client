# fibre-gateway-client

Client libraries and docs for the Fibre gateway, an HTTPS API that stores blobs on Celestia Fibre and reads them back.

## Why post to Celestia

[Celestia](https://celestia.org) is a data availability network. Posting a blob to it gives you more than storage:

- **Proof that your data was published.** A successful put records the blob's fingerprint on the Celestia chain.
  Anyone can check that this exact data was published, and when, without trusting you or us.
- **Availability backed by the network, not one server.** Your data is spread over Celestia's validators.
  A successful put means validators holding at least 2/3 of the stake have confirmed they store it.
  It stays retrievable even if some of them go offline.
- **References that can't be tampered with.** The `blob_id` is a fingerprint of your bytes.
  Anyone who reads the blob by its `blob_id` gets exactly what you put, or an error.

This is what rollups and other systems need when they must show that their data was made public and can be checked by others.

## Put and get

- **Put** sends one blob (32–128 MiB) and returns once it is available, with its `blob_id`.
  Our libraries check that the `blob_id` matches your bytes.
- **Get** returns the blob by its `blob_id` for 24 h after the put.

You need no Celestia account, keys or TIA: the gateway pays for every put.

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
