# Product verification API

The wallet and lending integrations open separate cases for one institution-issued customer reference. The wallet case verifies the ID and its face match, then screens the verified identity. Lending can reuse the original ID and face results. It requests proof of address and runs its own sanctions screen. An approval answers the verification requirements of the pinned policy. Lending owns the credit decision.

## Try the local journey

Start the Compose stack with the portal profile and open **Scenario lab**. Start a loan directly for a new customer, or onboard a wallet customer first and apply with the same subject. Complete any requested evidence. Expand the check details to compare the source cases and operation IDs. The automated journey uses separate product and reviewer credentials. It checks the four mock providers' own operation counters.

```sh
docker compose --profile portal up --build -d
python3 scripts/product_smoke.py
```

The demo issuer listens on loopback port 9805. Its synthetic issuance and scenario controls are for the local walkthrough. An institution deployment supplies its own authenticated evidence resolver through `EKYC_EVIDENCE_URL` and `EKYC_EVIDENCE_TOKEN`. Keep the worker ingress private to the service. The worker trusts the authenticated caller context forwarded by the API.

## Product and reviewer commands

Authenticate with a bearer token. Supply an `Idempotency-Key` for opening a case and for evidence, retry, and review commands. An accepted command returns HTTP 202 with a case ID and a `Location` header. Replay an identical command with the same key after a timeout. Conflicting reuse returns 409. Read the case to see its current requirements.

| Endpoint | Caller and behavior |
| --- | --- |
| `POST /v2/cases` | The product submits `use_case`, `subject_ref`, and an optional `evidence` map keyed by check. The service derives institution and product from its caller grant. |
| `GET /v2/cases/{id}` | The product owner, an authorized reviewer, or an observer granted that product can read state, policy version, reasons, next actions and check provenance. |
| `POST /v2/cases/{id}/evidence` | The owner submits `{"evidence":{"address":"<issuer reference>"}}` for an outstanding requirement. A correction uses a new evidence reference or version and a new command key. |
| `POST /v2/cases/{id}/retry` | The owner submits `{"check":"sanctions"}` to retry a blocked provider operation. Each logical attempt has a distinct operation ID. |
| `GET /v2/cases/{id}/review` | The external reviewer retrieves the current check's provider, operation ID, issuer evidence reference, reasons, late alerts with their original evidence references, and previous dispositions for this case. Use the provider operation ID in the authorized investigation system. |
| `POST /v2/cases/{id}/review` | A reviewer submits `check`, the exact `operation_id`, `disposition` (`clear` or `confirm`), a `reason` and the external `review_ref`. Clearing a match satisfies only that screening requirement. |

For example, wallet sends this payload after obtaining a document reference and a face reference bound to its portrait.

```json
{"subject_ref":"customer-42","use_case":"onboarding","evidence":{"document":"<issued document>","face":"<issued capture>"}}
```

Lending uses its own credential and sends the following payload. An eligible ID and face retain the original observation dates. An existing clear wallet screen still causes a new lending screen.

```json
{"subject_ref":"customer-42","use_case":"loan_application"}
```

`AWAITING_EVIDENCE` can be resumed by the product. `PROVIDER_BLOCKED` and `AWAITING_REVIEW` keep approval on hold and designate different actions. Check entries include their own status and next action when several requirements are outstanding. Decisions are terminal and retain the evaluated facts and policy.

## Read case lists and revisions

`GET /v2/cases` and `GET /v2/cases/overview` accept the same institution-scoped filters. `from` includes its RFC 3339 timestamp and `to` excludes it. The default creation window is seven days and the maximum is 366 days. Apply `product`, `use_case`, `state`, or a literal `query` on case ID or subject. Use the same explicit time bounds for a case list and its summary.

Lists support `sort=created_at_desc` or `created_at_asc`, with case ID as the tie-breaker. Pages start at one; `page_size` ranges from 1 to 100 (default 20). Narrow the window or filters to browse past the 10,000-row offset limit. The response contains the total matching count, current page and effective date range. Overview aggregates the whole filtered population.

`GET /v2/cases/{id}/history` orders public revisions newest first and returns `next_before_version` when another page exists. Pass that value as `before_version` to continue. Optional `from` and `to` apply to the revision’s recorded time. Point lookup by case ID can retrieve older cases independently of the list’s default window.

## Configure policies and the evidence issuer

The bundled policy definitions live in `crates/domain/policies`. To deploy an alternative catalog, mount the reviewed JSON files and point the worker's `EKYC_POLICY_DIR` at that directory. Supply the complete active catalog with one entry per product/use-case pair. Ship a new version identifier for every policy change. Each new case pins its full definition. The sample 90-day freshness periods, score thresholds and accepted methods are demo settings.

Configure `EKYC_CLIENTS` with distinct credentials. The demo reads the same grants from `EKYC_CLIENTS_FILE`. A product grant contains `access.institution`, `access.product`, `access.role="product"` and permitted `access.use_cases`. A reviewer grant uses `access.role="reviewer"`. The wallet and lending examples share an institution. A second institution can use identical customer references independently. The dashboard observer grant names an institution and an explicit list of readable products.

The worker sends an authenticated `POST /v2/resolve` to the configured issuer with `institution`, `subject_ref` and a map of requested references. The response contains:

- An `evidence` map with an immutable reference and revision, subject, tenant, kind, method, provider identity and provider-authorized reference per item. A face capture identifies the document revision and portrait revision compared.
- The current document and portrait revisions and, when available, a `screening` identity bound to that document. An empty input map still returns current bindings.
- HTTP 422 for a reference unavailable to this customer or for mismatched evidence. Dependency failures hold the affected work. Raw documents stay with the issuer.

Before publishing a lending approval that uses previous evidence, PostgreSQL locks that institution and subject and rechecks the pinned facts in the decision transaction. A newer adverse fact already committed at that point prevents reuse. The issuer lives outside this transaction. The approval uses the last resolved issuer snapshot, and a revision published by the issuer afterward requires a new verification or monitoring workflow. [Commit ordering and issuer freshness](upgrade-and-reuse.md) Configure production issuer identity, approved policy values and provider credentials for the intended deployment.

## Audit and verification

The trusted internal webhook receives `ekyc.case.action.v2`, `ekyc.case.decided.v2` and `ekyc.screening.alert.v2`. Deduplicate by `event_id` and use case versions to order snapshots. A late positive carries its originating operation ID and observation time for investigation. Internal case revisions retain original observations and reviewer dispositions. Use `GET /v2/cases/overview` for counts by state and by check kind, origin and status. `GET /v2/cases` pages through the same filtered case population. `GET /v2/cases/{id}/history` returns public snapshots in descending revision order.

To test worker recovery in the native disposable harness, set `EKYC_RECOVERY_TEST=1` when invoking `scripts/native-smoke.sh`. The harness stops its own worker after a provider saves the result, restarts it, and checks delivery of the final lending decision.

To exercise a late positive response after a loan closes, run the native harness with `EKYC_LATE_SCREEN_TEST=1 CHECK_TIMEOUT_MS=20000 CASE_DEADLINE_MS=45000`. The sanctions mock holds the first result until a reviewer confirms the replacement screening. Releasing the first result must produce one alert linked to its original evidence revision.

### Issuer wire contract and caller rotation

[Download the issuer OpenAPI contract](../apps/portal/public/issuer-contract.json). The examples include an empty lookup, a document/face pair, a proof of address, a binding mismatch and a temporary outage. Submit an empty evidence map to resolve current bindings, even for a new customer. Return all requested references together or HTTP 422. A temporary issuer outage returns 503. The bundled provider identities match their check kinds. For a configured vendor, send the exact identity used by its adapter. Bind the face capture to the same document and portrait revision. Retain the immutable evidence and timestamps necessary for review. The isolated release rehearsal runs a [second issuer implementation](../scripts/issuer_interop.py) against the real worker and all four provider adapters.

When rotating a product secret, replace only its token in the deployed grant. Preserve the caller ID, role, institution and product. Existing cases and opening-key replays depend on that ID. Verify the replacement can read an earlier case and replay the opening request to the same ID. Verify the retired token now returns 401. [Command recovery and rotation guide](../apps/portal/guide.md)

## Staff investigation intake

The institution’s assigned verification reviewers own investigation intake in **Staff workspace**. Production portal mode defaults to `staff`. Its proxy forwards the caller’s own bearer token to the API. Configure a unique reviewer caller ID and credential for each person and issue them through the institution’s secret-distribution process. Use HTTPS for the staff portal and its upstream API. Sign-out or reloading the browser discards the portal’s in-memory credential. Revoke access in the API grant configuration.

The local Compose portal explicitly enables `EKYC_PORTAL_MODE=demo` for the **Scenario lab** and the synthetic issuer. The **Staff workspace** link still requires a personal credential. For a local walkthrough, the two lending grants in `config/demo-clients.json` are `reviewer-alice` and `reviewer-sam`. Both can investigate lending cases, and the authoritative disposition stores the particular authenticated caller ID. Staff deployments should supply only their institution-issued grants and keep demo tooling disabled.

1. Open **Investigations** to see the current sanctions reviews and unacknowledged late screening alerts across every case creation date. Each item shows the original operation ID. Open **Inspect case** to read the reviewer handoff and compare it with the historical verification outcome.
2. Acknowledge intake with **Acknowledge investigation**. The first acknowledgement retains the reviewer and timestamp. Repeated acknowledgements return that receipt. An acknowledged screening review stays in **Open work** until its disposition. An acknowledged late alert moves to **Acknowledgement history**. Acknowledging a late alert leaves the terminal decision intact. Continue the investigation in the institution’s designated investigation system.
3. For a current sanctions match, enter the investigation reference, reason and disposition in the case inspector. A later reviewer can read the actor and original operation under **Recorded decisions**. Observers can use reports and public case history within their assigned products. Private handoffs, acknowledgements and dispositions require reviewer access.

Authenticate with `GET /v2/staff/session` to discover the actor, role and assigned products. `GET /v2/investigations` lists open work for a reviewer’s institution and product. Select `status=acknowledged` for acknowledgement history. Page with `limit` (1–100, default 25) and the opaque `next_after` returned on the previous page as `after`. Send `PUT /v2/investigations/acknowledgement` with the returned `work_id` in a JSON object. Keep the receipt’s original operation ID with the external investigation record.

### Native screen-reader acceptance

Build the portal from the repository root, then run the optional GNOME Orca check. It serves the built portal with two synthetic cases, drives the address correction and reviewer disposition by keyboard, and reads Orca’s speech transcript. It checks that focus returns to the case title, each decision is spoken once, and at least two subsequent case reads are silent.

```sh
pnpm --dir apps/portal install --frozen-lockfile
pnpm --dir apps/portal run build
podman build -f scripts/screen-reader/Containerfile -t localhost/ekyc-orca-test scripts/screen-reader
podman run --rm --network=none --shm-size=256m -v "$PWD/apps/portal/dist:/portal:ro" localhost/ekyc-orca-test
```

The test image provides Chromium, GNOME Orca, eSpeak, a virtual display and an audio sink. Docker accepts the same build and run commands. The portal build is mounted read-only. The separate release browser journey exercises the real service and reviewer credentials.
