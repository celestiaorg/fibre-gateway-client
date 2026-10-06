# Service contract

| Item | Promise |
|---|---|
| Endpoint | `https://cf.celestia-corto.com:8443`, round-robin over the instances serving traffic. Chain `corto-10` |
| Auth | Bearer tokens issued by Celestia |
| Blob size | 33,554,427 through 134,217,723 bytes, inclusive, excluding the 5-byte blob header added by the gateway ([blob sizes](client-guide.md#blob-sizes)) |
| Put | Returns only after the chain confirms the blob, with a receipt and a commitment proof |
| Availability | A `200` put means the blob is available: validators holding at least 2/3 of the stake signed that they store their pieces, and the chain accepted the payment. It stays readable for the retention period, even if some validators go offline |
| Get | Returns exactly the bytes that were put, with an exact `Content-Length` |
| Retention | Readable for 24 h after the put. After that `/v1/get` returns `404` |
| Put deadline | 150 s |
| Get deadline | 120 s |
| Capacity per instance | 48 concurrent puts and 32 concurrent gets. Above that, `429` at once |
| Payment | The gateway pays for every put, including retries |
| Idempotency | None. A retried put may store a second copy |

## Clients must

- Send exactly one `Authorization` header, and on put exactly one `Content-Type` header.
- Send `Content-Length` on put. Chunked uploads are rejected.
- Verify the commitment against your own data before trusting the receipt
  ([verification](client-guide.md#verification)).
- Keep the `blob_id`. `/v1/get` takes it.
- Use client timeouts of 160 s for put and 130 s for get.
- Back off with jitter on `429` and `503`.
- Reserve capacity before a job that needs more instances than are running, and wait `eta_seconds`.
- Treat `502`, `504` and network errors during a put as an unknown outcome, then retry with a new request.

## Clients must not

- Send blobs outside the size range, or expect padding to be stripped.
- Run more than 32 concurrent puts or 32 concurrent gets per instance without agreement.
- Let the HTTP library replay a put body by itself.
- Retry `400`, `401`, `404` or `413` without changing the request.
- Expect a blob to be readable more than 24 h after the put.
