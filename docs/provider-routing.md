# Address routing pilot

Routed address policies require exactly one address check. A future mixed identity and address policy must revalidate current issuer bindings and reused identity facts when an outstanding address result is settled. The pilot supports the configured adapter identities `address-a` and `address-b`. The demo institution enables three wallet use cases. Each case pins its complete product policy, including the two approved provider-method pairs and the thresholds used to interpret each result. The address mocks both require confidence above 80 and proof no older than 90 days. A policy version can set different thresholds for each method. Use separately calibrated thresholds for actual vendor scores.

| Use case | New requirement | Possibly accepted A submission |
| --- | --- | --- |
| `address-pilot` | A in buckets 0–79, B in 80–99 | Retrieve A using the original operation ID |
| `address-priority` | First admissible route in the order A, B | Retrieve A using the original operation ID |
| `address-replacement` | The same 80/20 cohort | Allow a linked B submission with its own ID and B locator. Wait for A before deciding |

## Issuer and placement

The worker requests `application/vnd.ekyc.issuer.v3+json` on the existing authenticated issuer endpoint. The mock also serves `/v3/resolve`. Version 3 returns an asset plus a list of full bindings for its candidate providers. Every candidate must match the institution, subject, check, evidence reference, revision and identity revisions of the requested asset. The worker checks the vendor against its configured endpoints and validates the method against the pinned policy. A v2 single-binding response authorizes only its named vendor. [Issuer contract](../apps/portal/public/issuer-contract.json)

For a synthetic capture, post an address to `/demo/evidence` with `providers` set to `["address-a","address-b"]` or a single authorized vendor. The issuer pins separate locators and methods during issuance. The fixture also accepts `provider_scenarios` when two vendors need to return different synthetic findings.

The percentage bucket is FNV-1a 64 over UTF-8 policy salt, one NUL byte, then the requirement identity, reduced modulo 100. The pilot salt is `address-pilot-v1`. The identity is the case ID, check kind and escaped immutable revision joined by slashes. If the evidence reference differs from the revision, append `#` and the escaped reference. Escaping replaces `%`, `/` and `#` with `%25`, `%2F` and `%23`. The mock uses the same UUID for reference and revision. Assignment history records the eligible IDs, chosen bucket or priority, policy version, restrictions, selected vendor, operation ID and predecessor. The binding table holds the protected locator.

The routing operator can drain a vendor so previously assigned operations finish, pause submissions including queued work, or cap the share. A committed authorization may already be in flight at the time of an override. Retrieval remains available for a submitted original. A possibly accepted operation stays outstanding until the vendor supplies an authoritative answer. The case writer keeps a late original address answer eligible after the submission deadline. A stale proof requires fresh evidence. Conflicting findings from the same revision go to review; the reviewer can confirm the conflict and require fresh proof. Both vendor answers remain in the operation history.

## Inspect a choice without sending

The following command accepts a frozen issuer sample on standard input. Set `DATABASE_URL` to a migrated pilot database containing the contract mapping. A read-only database account can run it. The selector evaluates the current scoped overrides, circuit and measured recovery window in one PostgreSQL read-only snapshot. The output includes the observed vendor from the sample and the prospective assignment.

```sh
cargo run -p ekyc-postgres --example address_shadow < config/address-shadow-sample.json
```

The command does not require vendor credentials or a running workflow worker. It exposes no evidence locators in its output. The sample exercises only synthetic bindings. [Sample input](../config/address-shadow-sample.json) · [Shadow selector](../adapters/postgres/examples/address_shadow.rs)

## Synthetic recovery settings

The A circuit starts recovery with a generation-fenced probe. A probe can compete after cooldown even with zero ordinary share. Following a successful probe, new eligible assignments ramp through 5%, 20%, 50% and 80%. Explicit recovery begins at 5%. Each stage uses completed A submissions since its start, capped at a trailing 60 seconds. Moving up one step requires at least four completed submissions, two seconds of dwell, p95 below 250 ms and zero transient errors. With four samples, p95 above 450 ms or more than 20% transient errors moves back one step. A verification finding counts as a completed response. The operator report shows the stage, cause, sample count, p95, error rate and window end. The independent vendor ledger records physical requests, including lookups. [Policy and selector](../crates/domain/src/product/routing.rs) · [Recovery measurement](../adapters/postgres/src/routing.rs)

## Enable or roll back

1. Apply the forward migrations and retain the previous service and original vendor endpoints for outstanding invocations. Give the pilot dedicated upstream accounts or drain legacy traffic sharing an account before cutover.
2. Install the approved vendor endpoints, contract mapping, credentials and scoped Restate rules. The synthetic stack sets `EKYC_ROUTING_INSTITUTION=demo-bank` and grants the three pilot use cases to the wallet caller. With that variable absent, the worker rejects opens using the address routing policy. Previously opened cases keep their pinned policy.
3. Run the shadow selector against representative authorized bindings, then open a new case with the selected pilot use case. Check the traffic report and independent mock ledger for the expected assignment, deadlines and charges.
4. To withdraw, remove the pilot use cases from new callers' grants and direct new cases to `address_refresh` using its corresponding issuer evidence. Drain the pilot vendors when existing assignments should finish. For an immediate stop on new outbound submissions, pause them instead. Keep worker registration, retrieval endpoints and PostgreSQL accounting available until the outstanding queue has an authoritative outcome or assigned recovery. Removing a grant or expiry of an override leaves the historical case and issued operation IDs intact.

The isolated verification suite runs [weighted placement and recovery](../scripts/address_routing_smoke.py) and [lost replies, linked replacements and recapture](../scripts/address_recovery_smoke.py). The PostgreSQL tests exercise placement replay, the pause/drain send boundary and recovery thresholds with fresh evidence windows.
