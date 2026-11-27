# Corto-1 background write load

`fibre-load` sends distinct blobs through the Rust gateway client. The client
checks each commitment before the command counts a successful write. Receipts
must name `corto-1`. This command does not check chain inclusion or generate read
load; use `put_verify_get` for the initial upload, chain check and read-back.

## Build and run

From `clients/rust`, build the command at the commit selected for the run:

```sh
cargo build --release --locked --features http --example fibre-load
```

Put the bearer token in a file with mode `0600`. Use one plain token with no name
prefix. Do not pass a token on the command line.

```sh
./target/release/examples/fibre-load \
  --endpoint https://GATEWAY-HOST:8443 \
  --token-file /path/to/gateway-token \
  --rate 1 --bytes 1048571 --concurrency 4 --duration 1800
```

The endpoint and token file are required. The other values above are defaults.
`--rate` is the total requested blobs per second across the fleet, not per worker.
`--concurrency` caps the total number of puts in flight, including verification.
`--duration` is the admission window in whole seconds; `0` runs until a signal.
The command accepts rates from 0.001 to 10,000 blobs/s, 1 to 1,024 workers, and
finite windows of up to one year. Payloads must be 131,067 to 134,217,723 bytes.

Confirm the deployed gateway limit and existing traffic before increasing the
concurrency. The command uses the SDK's HTTP/2 pool to send requests to each DNS
address in turn, even at low rates. The SDK refreshes that pool from DNS. Large
uploads can be slower over HTTP/2; this command targets steady background load.

## Admission and failures

There is no pending request queue. The command skips scheduled writes while all
workers are busy or the gateway asks it to wait. It also skips missed timer slots
instead of sending a catch-up burst. Every admitted write has a different payload.

Transient failures delay new submissions with jittered exponential backoff,
capped at 30 seconds. A longer `Retry-After` takes precedence. No failed upload is
replayed. A server error other than `503`, or a transport failure after connection,
counts as an unknown outcome: the gateway may have stored and paid for the blob.

Invalid credentials, other permanent HTTP rejections, the wrong chain, and a
failed commitment check stop admission and return exit code `1`. Invalid command
configuration or an unreadable/invalid token file returns `2`.

At the end of the window, or on SIGINT/SIGTERM, the command stops admission and
allows existing requests up to 170 seconds to finish. Unfinished requests count
as unknown outcomes, and a drain timeout returns `1`. A normal window or signal
returns `0`, even if individual requests failed; inspect the counters.
For systemd, use `Restart=no` and `TimeoutStopSec=180s`.

## Output and measurement

Stdout contains JSON lines: one `start`, one `put` per completed request,
`progress` every ten seconds during admission, and one final `summary`.
Successful `put` records include receipts for later chain checks. Error records
contain a fixed outcome and HTTP status, without server messages or credentials.

`stats` counters are cumulative for the run:

- `attempted` and `attempted_bytes`: admitted requests and payload bytes.
- `verified` and `verified_bytes`: successful puts after commitment verification.
- `errors`: all failed or unfinished attempts. `unknown` is a subset of `errors`.
- `skipped`: scheduled writes that were not admitted.
- `http_statuses`: counts of non-success HTTP responses, by status.

Latency includes the upload, gateway response and client commitment check.
Mean and maximum latency cover successful puts only. Counters include completions
during drain. `elapsed_seconds` includes drain; `admission_seconds` excludes it.
`verified_bytes_per_second` divides verified bytes by elapsed time. For interval
goodput, divide the change in verified bytes by the change in elapsed seconds.
Do not compare offered bytes directly with chain throughput as if all puts passed.

Keep the configuration, client commit, host details and UTC start/end times with
each result. Check load-host CPU, memory and network use alongside gateway and
chain metrics. Memory grows with blob size and concurrency, including proof work.

## Checks

```sh
cargo test --all-features --locked
cargo clippy --all-features --all-targets --locked -- -D warnings
```
