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

Workers share a PostgreSQL concurrency limit, dispatch rate limit and circuit breaker per check kind. Two vendors configured for the same kind share that budget. Sanctions result lookups have their own per-operation backoff. Every logical operation uses a stable ID across transport retries. Short-lived fenced leases protect provider concurrency across workers. Restate owns deadlines, timers and recovery. A vendor that accepts a request before a connection fails must recognize its operation ID on retry. The mock contracts implement that behavior.

The webhook relay leases a committed event, sends it outside its database transaction and acknowledges with a claim fence. A receiver may receive a duplicate after an ambiguous delivery and must deduplicate by event ID. The demo uses one trusted local destination. Production integrations need approved evidence providers, reviewed policy values, tenant-authorized event routing, protected secrets, retention rules, and deployment-specific recovery and capacity qualification. [Webhook relay](../adapters/webhook-events/README.md)

For sanctions, operations with an uncertain outcome have a durable result lookup. A positive recovered after closure enters the scoped investigation queue with the original operation and vendor timestamps. [Provider contracts](provider-contract.md)

## Deployment and migration

Drain v1 invocations and keep each earlier worker deployment available until its pinned invocations finish. Register the revised worker at a distinct URI. On a populated upgrade, the startup migrator locks the legacy tables and verifies full row equality in the retained schema before retiring the originals. The legacy rows retain their audit value and lack the issuer provenance required for v2 reuse. Existing v2 case history and queued events retain their original contracts. Export the archive, restore it to an empty database, and compare revision checksums. [Upgrade, restore and decision-time reuse](upgrade-and-reuse.md)
