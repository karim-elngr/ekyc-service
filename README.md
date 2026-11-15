# eKYC service

A Rust service for opening verification cases, collecting four kinds of provider evidence, and issuing a scoped, auditable decision. Restate processes commands and recovers interrupted work. PostgreSQL serves reads, coordinates provider quotas and stores completion events. Four independent local HTTP vendors and a webhook inbox exercise the actual network boundaries.

[Engineering design](docs/design.md) · [Provider fixtures](mocks/README.md) · [Webhook delivery](adapters/webhook-events/README.md)

## Run the local stack

Install Docker with Compose v2 and Python 3.10+. From the repository root run:

```sh
docker compose up --build -d
python3 scripts/smoke.py
```

The first build compiles the Rust workspace. Compose registers the worker with Restate before starting the API. The smoke test submits synthetic cases, polls decisions and verifies each terminal event reached the demo webhook. The API is at `http://127.0.0.1:8090`, Restate's local admin UI at `http://127.0.0.1:9070`, and the authenticated mock inbox at `http://127.0.0.1:8110`. Sample tokens in Compose are for this loopback demo.

### Submit a case

```sh
curl -i http://127.0.0.1:8090/v1/cases \
  -H 'Authorization: Bearer local-demo-onboarding' \
  -H 'Idempotency-Key: first-case' \
  -H 'Content-Type: application/json' \
  --data '{"subject_ref":"synthetic-customer","purpose":"full_onboarding",
    "evidence":{"document":"demo:pass","face":"demo:threshold",
                "address":"demo:pass","sanctions":"demo:pass"}}'
```

Follow the returned `Location` using the same bearer token. Once the case is `DECIDED`, this example returns `MANUAL_REVIEW` with `FACE_SIMILARITY_LOW`. For an approved case, use a new key and set the face reference to `demo:pass`. Reusing the first key with changed evidence returns HTTP 409. A different caller cannot read this case.

```sh
curl -H 'Authorization: Bearer local-demo-onboarding' \
  "http://127.0.0.1:8090/v1/cases/$CASE_ID"
```

`docker compose down` stops the services and preserves the named data volumes. Use `docker compose logs -f worker webhook` for local diagnostics. A fresh empty database starts automatically from the checked-in migrations.

## Build and test

The native stack was exercised with Rust 1.100, Restate Server 1.8.1, Restate Rust SDK 0.13.0 and PostgreSQL 17. The workspace commits its Cargo lockfile. Release images compile both Rust services and the independent webhook relay. The fixture tests require only Python.

```sh
cargo build --locked --workspace
cargo test --locked --workspace
python3 -m unittest discover -s mocks -v
```

For SQL concurrency and fencing tests, use a disposable PostgreSQL database and a test DB user that can create databases:

```sh
cargo test --locked -p ekyc-postgres --features postgres-tests
```

SQLx reads the connection from `DATABASE_URL`. A native integration harness is available for an already-running disposable Restate Server and PostgreSQL. It launches its own API, worker, relay, four vendors and inbox, then stops those application processes. Use a fresh Restate instance and database for this test. The default native ports must be free.

```sh
# DATABASE_URL points to your disposable local PostgreSQL database.
# Restate ingress defaults to :8080 and admin to :9070.
cargo build --locked --workspace
bash scripts/native-smoke.sh
# To exercise a real worker crash during provider I/O on Linux:
EKYC_RECOVERY_TEST=1 bash scripts/native-smoke.sh
```

## Repository layout

| Path | Responsibility |
| --- | --- |
| `crates/domain`, `crates/ports` | Verification policy, scope and evidence types, and the provider boundary |
| `adapters/provider-{document,face,address,sanctions}` | One crate per provider, each with its own vendor protocol |
| `crates/http-transport` | Connection pooling, timeouts, bounded bodies and HTTP error classification |
| `adapters/restate`, `adapters/restate-ingress` | Command handlers and a separate caller-side ingress client |
| `adapters/postgres`, `adapters/webhook-events` | Query projections, audit, shared limits, outbox and delivery |
| `apps/http-api` | Authenticated command and query endpoints |
| `mocks`, `scripts` | Synthetic vendor fixtures, webhook inbox, registration and integration tests |
