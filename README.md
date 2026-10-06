# fibre-gateway-client

Client libraries and docs for the Fibre gateway, an HTTPS API that stores blobs on Celestia Fibre and reads them back.

Start with **[docs/getting-started.md](docs/getting-started.md)**. Then:

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
