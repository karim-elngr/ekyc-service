## Open a verification case

Each case belongs to one product and one customer. The credential determines the institution, product and role. The wallet offers `onboarding`, `full_onboarding` and `address_refresh`. Lending accepts `loan_application` directly or after a wallet verification.

Open the [scenario lab](/#lab) to try the workflows using synthetic evidence and the portal's server-side credentials. In Scalar, select an operation and enter your own authorized bearer credential to test the API.

1. Obtain issuer references for the customer. A face capture binds to the current document portrait. Screening uses the verified identity. 
2. Send `POST /v2/cases` with a stable `Idempotency-Key`, `subject_ref`, `use_case` and any available `evidence`. Each product pins its own policy.
3. Read the case at the returned `location`. Each check reports its state, original case, provider operation and any next action. A loan may reuse eligible wallet document and face observations, request a proof of address and run a new sanctions check.

A direct loan can start with all evidence or request missing items later. The following request lets lending assess existing evidence and ask for any remaining requirements.

```json
{"subject_ref":"customer-42","use_case":"loan_application"}
```

## Respond to the case

| Next action | How to continue |
| --- | --- |
| Provide evidence | Submit issuer references to `POST /v2/cases/{id}/evidence`. A corrected item needs a new reference or revision. |
| Wait for provider | Refresh the case or consume case updates. In an interactive lab scenario, read `/interactive` and send the chosen callback for that operation. |
| Retry | After the provider recovers, post the check kind to `/retry`. A persistent outage can block another attempt. |
| Review | An assigned reviewer reads `/review` for the original evidence and current operation, then submits a disposition, investigation reference and reason. |
| Terminal decision | Read the final checks and policy. A late positive screening alert can still require investigation. |

Use one idempotency key and the same body when retrying an uncertain case command. A 409 can indicate a changed operation, conflicting earlier input or a case state that no longer permits that command. Read the latest case or reviewer handoff before making a new decision.

## Report and investigate

Use `GET /v2/cases` for paginated case search and `GET /v2/cases/overview` for totals across the entire filtered population. Both support an inclusive creation-time start, exclusive end, product, use case, status and a case or customer reference search. The default is seven days and the maximum window is 366 days. Use `/history` to read earlier case revisions with an exclusive version cursor.

Product credentials read their own cases. An institution observer can read public cases and history across assigned products. Reviewer-only handoffs contain evidence references and investigation records. The local portal selects credentials on the server. The interactive API guide passes through the credential entered by its reader.

## Receive events

The internal receiver accepts action updates, terminal decisions and late screening alerts. Authenticate the relay, deduplicate by `event_id`, and compare case versions when applying snapshots that arrive out of order. Alert IDs refer to the originating provider operation. Use the authorized reviewer handoff to investigate.

[Download the OpenAPI 3.1 document](/openapi.json) for the complete service contract.

## Integrate an evidence issuer

The institution implements the [issuer request and response contract](/issuer-contract.json). The paired examples cover a new customer, a document and face capture, and proof of address. For a known subject, an empty request still returns the current bindings and screening identity. Resolve every requested immutable reference in the same subject and institution. Return 422 for a mismatch and 503 for a temporary dependency failure. The default adapters accept the provider identities `document`, `face`, `address` and `sanctions`. Both the issuer and worker must agree on vendor identity and method.

## Recover from a command conflict

| HTTP 409 code | What to do |
| --- | --- |
| `idempotency_key_reused` | Recover the original command body and replay it with the original key. For a genuinely different intent issue a new key. |
| `stale_operation` | Read the reviewer handoff again and verify the current operation before submitting a new disposition. |
| `case_already_decided` | Read the terminal verification and its history. An accepted earlier command may still be replayed with its original key. |
| `invalid_case_state` | Read the per-check next action and submit a command permitted in the current state. |

Keep the configured caller `id` when rotating a bearer token. Deploy the replacement secret under the original institution, role and product grant. The caller ID scopes both ownership and opening keys. After cutover verify point lookup and replay of an earlier opening request. Remove the old secret from clients and confirm it is rejected.

## Check service availability

`GET /health/reads` checks database-backed reads. `GET /health/ready` also invokes the current command deployment and performs an authenticated empty lookup at the issuer for the reserved institution `system-health` and subject `readiness`. Return an empty binding for that synthetic lookup. Both probes return HTTP 200 when available and 503 otherwise.

## Investigate current and late screening results

Open **Staff workspace** and sign in with your personal institution-issued credential. Reviewers see **Investigations** for their assigned product, across all case creation dates. Each entry links its original screening operation to the case. **Acknowledge investigation** records the first actor and time. Find receipts under **Acknowledgement history**. A current match remains open until a reviewer records a disposition. A late alert on a closed case keeps the historical verification decision intact. Observers can use **Overview**, **Cases** and public history within their assigned products.

For programmatic intake, read `GET /v2/staff/session` for your scope, then page through `GET /v2/investigations`. Acknowledge with `PUT /v2/investigations/acknowledgement` and its `{"work_id":"…"}` body. Repeating the request returns the original acknowledgement. Keep its operation ID in the external investigation record.
