# Releasing

One `v`-prefixed [semver](https://semver.org) tag (`v0.2.0`) versions everything in this repo together:

- the Go module `github.com/celestiaorg/fibre-gateway-client` (the OpenAPI spec and test vectors),
- the Rust crate [`fibre-gateway-client`](https://crates.io/crates/fibre-gateway-client), whose version in
  `clients/rust/Cargo.toml` must equal the tag,
- the Python reference verifier in `clients/python`, which isn't packaged.

While the major version is 0, a minor bump may contain breaking changes. PR titles follow
[Conventional Commits](https://www.conventionalcommits.org) and become the squash-merge commit on `main`.

## Cutting a release

1. Open a PR bumping `version` in `clients/rust/Cargo.toml` (and `Cargo.lock`), and merge it.
2. Tag that commit on `main` and push the tag:

   ```sh
   git tag -s v0.2.0 -m v0.2.0
   git push origin v0.2.0
   ```

3. The `release` workflow:
   - checks the tag matches `Cargo.toml`,
   - publishes the crate to crates.io through trusted publishing, skipping versions that are already published,
   - creates a GitHub release with generated notes. Tags with a suffix (`v0.3.0-rc.1`) become pre-releases.

`go get github.com/celestiaorg/fibre-gateway-client@v0.2.0` works as soon as the tag is pushed.

## One-time setup

- On crates.io, add a trusted publisher for `fibre-gateway-client`: repository `celestiaorg/fibre-gateway-client`,
  workflow `release.yml`, environment `crates-io`.
- In this repo, create the `crates-io` environment, optionally restricted to `v*` tags or with required reviewers.
