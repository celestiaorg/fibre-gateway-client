# API reference: `/v1/put`, `/v1/get` and `/v1/capacity`

The wire format of the two public endpoints.
The same endpoints are described as an [OpenAPI 3.1 spec](openapi.yaml).
For how to use them (tokens, retries, verification), read the [client guide](client-guide.md).
For what we promise, read the [service contract](contract.md).

## Basics

| Item | Value |
|---|---|
| Base URL | `https://cf.celestia-corto.com:8443` |
| Endpoints | `POST /v1/put`, `POST /v1/get`, `POST` and `GET /v1/capacity`. Any other path or method returns `404 not_found` |
| Auth | Exactly one header, `Authorization: Bearer <token>` (see [Tokens](client-guide.md#tokens)) |
| Transport | HTTPS only, TLS 1.2 or newer. Plain HTTP gets a plain-text `400` |
| Errors | JSON `{"error":"<code>"}` with `Content-Type: application/json` (see [Errors](#errors)) |
| Caching | Every response has `Cache-Control: no-store` |

One put stores one blob in one Celestia transaction.
There is no deduplication: two puts of the same bytes store two copies.

## `POST /v1/put`

Stores one blob. The response comes after the chain confirms the blob, so a put takes seconds.

### Put request

| Item | Value |
|---|---|
| `Content-Type` | `application/octet-stream`, exactly one header |
| `Content-Length` | Required. 33,554,427 through 134,217,723 bytes (32 MiB − 5 through 128 MiB − 5). Chunked uploads are rejected |
| Query string | None allowed |
| Body | The blob bytes |

The size is checked before the body is read.

### Put response

`200`, `Content-Type: application/json`, exactly these five fields:

```json
{"chain_id":"corto-10","tx_hash":"<64 hex>","blob_id":"<66 hex>","promise_height":123,
 "commitment_proof":{"row_root_siblings":["<64 hex>","<64 hex>"]}}
```

| Field | Format | Meaning |
|---|---|---|
| `chain_id` | string | The chain, `corto-10` |
| `tx_hash` | 64 hex characters | The `MsgPayForFibre` transaction that paid for the blob |
| `blob_id` | 66 hex characters | Version byte `00`, then the 32-byte [commitment](client-guide.md#terms) |
| `promise_height` | integer, 1 to 2^63 − 1 | Height of the payment promise. Not the block that included the transaction |
| `commitment_proof.row_root_siblings` | two 64-hex strings | Lets you recompute the commitment from your data ([verification](client-guide.md#verification)) |

The first four fields are the **receipt**. After you [verify](client-guide.md#verification) the commitment, store only `blob_id`; it is all `/v1/get` needs.

## `POST /v1/get`

Returns a whole blob.

### Get request

The body is a JSON object with `blob_id`, at most 4 KiB.
Send `Content-Type: application/json`.

```json
{"blob_id": "000123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123"}
```

The whole put response is also accepted.

Rules, all enforced:

- `blob_id` is required.
- `chain_id`, `tx_hash`, `promise_height` and `commitment_proof` are optional. If present, they must be valid.
  `commitment_proof` must be exactly `{"row_root_siblings":["<64 hex>","<64 hex>"]}`; the gateway checks its shape and ignores it.
- Unknown, duplicate or `null` fields, and data after the object, are rejected.
- `chain_id` must be the gateway's chain. `tx_hash` must be 64 hex characters.
  `promise_height` must be a JSON integer from 1 through 2^63 − 1.
- `blob_id` must use a supported version (`00`).

The gateway does not check `tx_hash` against the chain on a get.
It does check every piece it downloads against `blob_id`.
`promise_height`, if sent, selects the validator set to read from; without it the gateway uses the current set.

### Get response

`200`, `Content-Type: application/octet-stream`, with an exact `Content-Length`.
The body is the whole blob, exactly the bytes you put. Blobs can be read for 24 h after the put.

## `/v1/capacity`

Reserves gateway instances before a large job. When to use it: [Capacity](client-guide.md#capacity).

| Request | Body | Response |
|---|---|---|
| `POST /v1/capacity` | `{"instances":<3–20>,"minutes":<1–120>}`, `Content-Type: application/json`, at most 1 KiB. Unknown fields are rejected | `200` with the status below |
| `GET /v1/capacity` | None | `200` with the status below |

```json
{"floor":20,"expires_at":"2026-10-04T13:00:00+00:00","target":20,"active":8,"running":12,"eta_seconds":150}
```

`floor`, `target`, `active`, `running` and `eta_seconds` are integers. `expires_at` is RFC 3339, or `null` with `floor` `0`.

## Errors

| Status | `error` | Endpoint | Meaning |
|---|---|---|---|
| 400 | `invalid_request` | put | `Content-Type` is missing, wrong or repeated, or there is a query string |
| 400 | `invalid_body` | put | Body shorter than `Content-Length`, or not received before the deadline |
| 400 | `invalid_request` | get | Body over 4 KiB, malformed JSON, missing or extra field, bad `commitment_proof`, wrong chain, bad hash or height, unsupported blob version |
| 400 | `invalid_request` | capacity | Malformed JSON, unknown field, or `instances` or `minutes` out of range |
| 401 | `unauthorized` | all | Token missing, wrong, retired, or more than one `Authorization` header |
| 403 | `forbidden` | capacity | This token may not reserve capacity |
| 404 | `not_found` | get | Blob not found. Older than 24 h, or the `blob_id` is wrong |
| 404 | `not_found` | all | Unknown path, or a method other than `POST` (or `GET` on capacity) |
| 413 | `invalid_length` | put | `Content-Length` missing or outside the size range |
| 429 | `capacity` | put, get | All slots for this endpoint on this instance are busy. Not queued |
| 429 | `rate_limited` | capacity | The reservation changed less than 10 s ago. Retry later |
| 502 | `upload_failed` | put | Upload or chain confirmation failed. Outcome unknown |
| 502 | `receipt_unavailable` | put | Confirmed, but the gateway could not look up the transaction. Outcome unknown |
| 502 | `proof_unavailable` | put | Confirmed and paid, but no commitment proof. Report it |
| 502 | `download_failed` | get | The blob could not be downloaded |
| 503 | `unavailable` | all | The instance is shutting down |
| 503 | `capacity_unavailable` | capacity | The capacity service did not answer. Retry with backoff |
| 504 | `timeout` | put | Not confirmed within the deadline. Outcome unknown |
| 504 | `timeout` | get | Not downloaded within the deadline |

What to do for each one is in [Errors and retries](client-guide.md#errors-and-retries).
