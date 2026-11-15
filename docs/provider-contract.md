# Connect a verification provider

The evidence issuer selects a vendor by its stable `provider` identity for each document, face, address or sanctions item. Set `EKYC_PROVIDER_CATALOG` on the worker to a JSON object keyed by check kind, then by vendor identity. Each URL is the base for that adapter's wire protocol.

```json
{
  "document": {"identity-a":"https://identity-a.example","identity-b":"https://identity-b.example"},
  "face": {"face-a":"https://face.example"},
  "address": {"address-a":"https://address.example"},
  "sanctions": {"screen-a":"https://screen.example"}
}
```

During a switch, register both vendor identities and have the issuer assign the new identity to new evidence. Keep the old route available until active checks and outstanding screening lookups for that vendor finish. Never reassign an existing identity to a different vendor. Reviewer handoffs retain the original provider and method. If the catalog is absent, the four existing `DOCUMENT_URL`, `FACE_URL`, `ADDRESS_URL` and `SANCTIONS_URL` settings register the legacy identities `document`, `face`, `address` and `sanctions`.

Every vendor must honor the operation ID in the `Idempotency-Key` header across retries and restarts. Successful responses include an RFC 3339 `evaluated_at` fixed when the vendor evaluated that operation. Return the original value on cached delivery. The worker records receipt separately, uses receipt time for the operation deadline, and uses vendor evaluation time for ordering and reuse. A malformed or missing timestamp cannot authorize a check. A future timestamp blocks the check. Legacy passes with only local receipt time do not establish reusable identity evidence. Unchanged historical decisions remain readable.

| Operation | Request | Successful response fields |
| --- | --- | --- |
| `POST /documents/verify` | `document_ref` | `evaluated_at`, `status`, `confidence`, `expires_on` |
| `POST /faces/compare` | `capture_ref` | `evaluated_at`, `match_status`, `confidence`, `similarity_score` |
| `POST /addresses/verify` | `proof_ref` | `evaluated_at`, `status`, `confidence`, `proof_date` |
| `POST /screenings` | `identity_ref` | `evaluated_at`, `screening_status`, `match_count` |

## Reconcile an uncertain sanctions operation

The worker sends `POST /screenings/result` to the original vendor with the original operation ID in both the header and the JSON field `operation_id`. This endpoint must never start another screening. Responses use HTTP 200 and a JSON object with a `status` field. For `pending`, `not_accepted` and `inconclusive`, return just that field. A complete response contains the original result.

```json
{"status":"complete","result":{"screening_status":"HIT","match_count":1,"evaluated_at":"2026-11-15T09:00:00Z"}}
```

Only return `not_accepted` once the vendor can rule out a prior acceptance **and prevent a delayed submission for that operation from being accepted later**. Return `pending` if acceptance or the result is still uncertain. An authoritative `inconclusive` closes the lookup without a verdict. A missing route, 404, invalid payload, or transport failure leaves the lookup pending. The worker durably schedules another lookup with backoff. Operators can inspect the `ProviderChecks/reconcile` invocations in Restate and must retain the vendor route and its operation records while work is outstanding.

Reconciliation continues after case closure. A recovered positive for an expired or superseded operation creates one investigation event under `case_id:alert:operation_id`, preserving the earlier decision and evidence. The event and reviewer handoff include vendor observation time and, for new deliveries, worker receipt time. A receiver still deduplicates retransmissions of that event. The disposable native harness exercises this with `EKYC_RECONCILIATION_TEST=1`.

## Local callback adapters

Compose and the native demo explicitly set `EKYC_DEMO_CALLBACKS=1` on the API and worker. Only the four legacy demo adapter identities interpret the synthetic interactive fixture. Custom vendor identities and deployments with that setting off treat provider references as opaque. Deploy new worker definitions under a distinct Restate deployment and retain previous deployments until their in-flight invocations have drained.
