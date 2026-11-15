# Local provider fixtures

Each mock listens on its own port and accepts its vendor's request schema. Use separate processes for the four services or start the project's Docker Compose stack. A standalone document provider can be started from the repository root with:

```sh
python3 mocks/server.py document --host 127.0.0.1 --port 9801
```

Each direct verification endpoint requires an `Idempotency-Key` header. Reuse the same key when retrying a logical operation. The mock caches its terminal response and returns HTTP 409 if the same key is used for a different supported evidence reference. Terminal results and their immutable vendor evaluation timestamps survive process restart when the mounted SQLite database is retained. Standalone mocks use in-memory storage unless you set `PROVIDER_DB_PATH` or `INTERACTIVE_DB_PATH`. Use a new key to restart a fixture's retry sequence.

| Provider | HTTP endpoint | JSON request field |
| --- | --- | --- |
| document | `POST /documents/verify` | `document_ref` |
| face | `POST /faces/compare` | `capture_ref` |
| address | `POST /addresses/verify` | `proof_ref` |
| sanctions | `POST /screenings` | `identity_ref` |

| Synthetic reference | Behavior |
| --- | --- |
| `demo:pass` | Passing scores and dates, or a clear sanctions screen |
| `demo:review` | Review status for document, face and address. Sanctions returns HTTP 422 for an inconclusive screen |
| `demo:threshold` | Confidence exactly 85 for document, similarity exactly 85 for face, or confidence exactly 80 for address |
| `demo:expired` | Document expired yesterday |
| `demo:stale` | Address proof was issued 91 days ago |
| `demo:hit` | Sanctions screen returns one hit |
| `demo:inconclusive` | Sanctions screen returns HTTP 422 |
| `demo:throttle-once` | First call returns HTTP 429 with `Retry-After: 1`, then passes |
| `demo:unavailable-once` | First call returns HTTP 503, then passes |
| `demo:unavailable` | Every call returns HTTP 503 |
| `demo:timeout-once` | The result is saved, then the first response is delayed by ten seconds (configurable with `--timeout-delay`). A retry with the same key immediately receives the saved result |

Each successful result includes an RFC 3339 `evaluated_at` set on first processing. Dates are relative to that UTC day. The sanctions fixture also provides authoritative lookup of a previous operation at `POST /screenings/result`. [Vendor contract, configuration and reconciliation](../docs/provider-contract.md) Unknown fixtures return HTTP 422. Each server exposes `GET /health`. The local-only `GET /__mock__/operations/{encoded_operation_id}` returns attempt and cache-hit counts for the worker-restart test. Run the loopback HTTP tests from the repository root with:

```sh
python3 -m unittest discover -s mocks -v
```

## Interactive provider callbacks

With `RESTATE_INGRESS_URL` configured, each mock also accepts interactive checks. The worker registers a case, an exact provider operation ID and a Restate awakeable through `POST /__mock__/interactive/register`. HTTP 202 acknowledges the registration. The portal can then read the check through the case API and select a provider response.

| Mock endpoint | Behavior |
| --- | --- |
| `GET /__mock__/interactive/{case_id}?operation_id={operation_id}` | Reports `awaiting_response`, `sending` or `sent` and the selected response, if any |
| `POST /__mock__/interactive/{case_id}?operation_id={operation_id}` | Accepts `{"response":"demo:pass"}` or another supported final response. Repeating the choice is safe. A different choice returns HTTP 409. |

The mock saves the selected response and its exact vendor payload before acknowledging the choice. It sends the payload, including the simulated vendor HTTP status, to Restate's awakeable resolution endpoint. A successful delivery resumes the provider invocation. If Restate is unavailable, delivery retries with backoff. Each mock uses its own SQLite file at `INTERACTIVE_DB_PATH` (default `/tmp/ekyc-interactive-{provider}.sqlite3`). Retaining this file preserves pending checks and undelivered callbacks after a restart.
