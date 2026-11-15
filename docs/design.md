# Verification architecture

## Decision ownership

A case answers a verification question for one institution, customer, product and use case. Product credentials select the use cases they may open. Each case pins its complete policy and maintains a monotonically increasing revision. The product decides what to do with the resulting eKYC decision. The four bundled policies and their scopes are listed in the [README](../README.md).

## Command path

The HTTP API authenticates the caller, validates the command and derives a caller-scoped case ID from the opening idempotency key. It submits commands to a Restate `ProductCase` object. The object serializes case changes, tracks receipts for replay and dispatches independent provider operations. Provider jobs run outside the case lock. A repeated command with a different payload yields a conflict.

The evidence issuer resolves opaque references within the given institution and subject. A face capture identifies the document and portrait revisions it was taken against. The pure domain applies the pinned policy to provider observations. Eligible identity checks may retain the original provider operation and observation time on a new case. Binding, source, method, freshness and policy assessments must agree. Replacing the document invalidates dependent identity evidence; each loan screens its own verified identity.

The evaluator checks expiry and proof dates in UTC, treats the configured score thresholds as strict lower bounds, and requests corrections for unusable evidence. A possible sanctions match goes to a scoped reviewer for clearance or confirmation. A failed provider operation can be retried. Interactive local scenarios register an operation-specific Restate callback with the mock provider and resume the same case when an operator selects a response.

## Persistence and reads

PostgreSQL atomically publishes the current public projection, immutable assessment revisions, reusable facts, and applicable action or decision events. A late positive screen raises an investigation alert linked to its original operation. The API exposes the public projection to the case owner, scoped reviewers and explicitly granted observers. Reviewer investigation details require a separate grant. Case history public responses exclude private artifacts.

The case directory and overview share the same creation-time interval, text and structured filters and authorization scope. SQL performs the filtering, aggregates and pagination before responding. Explicit timestamps keep consecutive pages within the same creation window. Cases are ordered by creation timestamp and case ID. The page count and contents come from the same query snapshot. Mutable state can still change between HTTP requests. History uses decreasing version numbers to page through immutable revisions.

## Recovery and delivery

Restate runs each provider attempt under a contract-specific concurrency pool with separate submission and lookup lanes. PostgreSQL admits the physical call under the configured rolling quota, vendor breaker and billing account. An evidence revision can bind to several authorized vendors. A pinned policy chooses among those bindings and commits the assignment before dispatch. Two routes can consume the same upstream quota while keeping distinct breakers. Each logical operation keeps a stable vendor ID across transport retries. [Provider contract](provider-contract.md) · [Demo capacities](../config/provider-contracts.json)

For the opt-in wallet address pilot, an 80/20 split is computed from the pinned policy and requirement identity. Draining a vendor routes eligible new cases away while admitted work finishes. A paused vendor can still answer an original-operation lookup. Each replacement has its own authorized binding, operation and predecessor. Staff can inspect the assignment reason, operation status, owner, billing reservations and recovery stage. The local recovery thresholds use small sample windows to make an outage reproducible. [Policy, shadow evaluation and rollback](provider-routing.md) · [Address pilot rehearsal](../scripts/address_routing_smoke.py)

The opt-in strict lending policy records an obligation for the original sanctions operation. A replacement clear can arrive while that obligation remains open. Approval waits for the original result or an authorized disposition. The existing lending policy keeps its late-hit investigation workflow. [Strict screening rehearsal](../scripts/strict_screening_smoke.py)

The webhook relay leases a committed event, sends it outside its database transaction and acknowledges with a claim fence. A receiver may receive a duplicate after an ambiguous delivery and must deduplicate by event ID. The demo uses one trusted local destination. Production integrations need approved evidence providers, reviewed policy values, tenant-authorized event routing, protected secrets, retention rules, and deployment-specific recovery and capacity qualification. [Webhook relay](../adapters/webhook-events/README.md)

For sanctions, operations with an uncertain outcome have a durable result lookup. A positive recovered after closure enters the scoped investigation queue with the original operation and vendor timestamps. [Provider contracts](provider-contract.md)

## Deployment and migration

The local stack registers the new worker at `worker-cases-routing1`. Before replacing a populated Compose deployment, drain pinned invocations or keep the previous worker image running at its original URI until they finish. Register the revised worker at a distinct URI. On a populated upgrade, the startup migrator locks the legacy tables and verifies full row equality in the retained schema before retiring the originals. The legacy rows retain their audit value and lack the issuer provenance required for v2 reuse. Existing v2 case history and queued events retain their original contracts. Export the archive, restore it to an empty database, and compare revision checksums. [Upgrade, restore and decision-time reuse](upgrade-and-reuse.md)
