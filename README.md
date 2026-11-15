# eKYC verification service

Wallet and lending integrations use one case API and one policy engine. A first-time loan applicant can open a lending case immediately. A returning wallet customer can supply the same institution-issued subject reference to reuse eligible ID and face checks. Every loan application requires its own sanctions screen and proof-of-address check.

| Product | Use case | Required evidence and checks |
| --- | --- | --- |
| Wallet | `onboarding` | Document, face match and sanctions |
| Wallet | `full_onboarding` | Document, face match, address and sanctions |
| Wallet | `address_refresh` | Proof of address |
| Lending | `loan_application` | Document and face (fresh or eligible for reuse), address and sanctions (fresh) |

## Run locally

```sh
docker compose --profile portal up --build -d
```

Podman Compose runs the same stack. Open the [verification portal](http://127.0.0.1:5173). **Scenario lab** opens cases, issues synthetic evidence and exercises provider and reviewer actions. **Overview** and **Cases** read the same scoped case data. The [API guide](http://127.0.0.1:5173/docs/) includes the interactive contract.

The API listens on loopback port 8090, the local issuer on 9805, the webhook inbox on 8110 and Restate administration on 9070. Local product, reviewer and dashboard grants live in [the demo configuration](config/demo-clients.json). The portal proxy supplies its demo credentials on the server. The API guide uses the credential entered by its reader.

To start a lending case before collecting evidence:

```sh
curl -i http://127.0.0.1:8090/v2/cases \
  -H 'Authorization: Bearer local-demo-lending-product' \
  -H 'Idempotency-Key: direct-loan-001' \
  -H 'Content-Type: application/json' \
  --data '{"subject_ref":"customer-42","use_case":"loan_application"}'
```

Follow the returned location. Submit issuer references to its `/evidence` endpoint as the applicant completes the requirements. An approved lending case satisfies the verification policy; the lender owns underwriting.

### Complete a first case

From the repository root start the stack and run the copyable walkthrough with Python 3.10 or newer.

```sh
docker compose --profile portal up --build -d
python3 scripts/first_case.py
```

The walkthrough creates a unique customer, opens a loan with no evidence, follows the returned location, issues a document, matching face capture and address, and polls the public API. Its final output shows verification `APPROVED` and four fresh `PASSED` checks. The lending product still owns underwriting. [Full executable request sequence](scripts/first_case.py) · [Issuer wire contract](apps/portal/public/issuer-contract.json) · [Presenter deck](workflow-orchestration/index.html)

## Staff and presenter walkthroughs

Open **Staff workspace** and sign in as `local-demo-reviewer-alice` or `local-demo-reviewer-sam` to investigate lending cases. For the address pilot, sign in with `local-demo-routing-operator` and open **Provider routing** to inspect assignments, set a time-limited override and see outstanding operations. The local observer credential is `local-demo-institution-observer`. The demo uses synthetic data. The institution supplies its own identities for staff deployments. [Staff intake and acknowledgement](docs/product-verification.md#staff-investigation-intake)

The engineering talk opens with a lost provider reply. It follows ownership of the next step, durable waits, replay, overload and the point where recovery depends on the external service. The eKYC demo gives the audience a running workflow and an invocation journal to inspect. [Presenter deck on the default local port](http://127.0.0.1:5173/talk/)

## Explore the database schema

Open **Database schema** in the demo portal, or follow the link on the architecture slide. In Liam, search for `decision_outbox`, select the table, and inspect **Foreign key** to see how each published event refers to one `product_case_revision` by case ID and version. Select `provider_attempt` to trace each admitted request to its quota bucket, breaker and spend reservation. [Local Liam viewer](http://127.0.0.1:5173/schema/) · [Liam ERD](https://github.com/liam-hq/liam)

The portal build generates the diagram from a schema-only PostgreSQL export. The release check compares its tables, columns and foreign keys with the migrated schema. The underlying SQL also defines the `staff_current_work` view. No case records or database credentials are needed to browse. [Schema source](apps/portal/schema/structure.sql)

After applying migrations to a running demo, refresh the source and rebuild the portal. For a side-by-side deployment, set `COMPOSE_PROJECT_NAME` to its project name for both commands. With Podman, use `CONTAINER_ENGINE=podman make schema-refresh` and `podman compose`.

```sh
make schema-refresh
docker compose --profile portal up -d --build --no-deps portal
```

## Release verification

Use Python 3.10 or newer, Rust with rustfmt and Clippy (the container build uses Rust 1.100), and Docker with Compose or Podman with a Compose provider. From the repository root run the following commands. A working container engine, free local ports and enough disk space for the images are required.

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r scripts/requirements-qa.txt
.venv/bin/python -m playwright install chromium
make verify PYTHON=.venv/bin/python
```

`make test` runs the same release check. To use an installed Chrome or Chromium, set `EKYC_BROWSER_EXECUTABLE` to its executable instead of downloading Playwright’s Chromium. Set `CONTAINER_ENGINE=podman` to select Podman explicitly.

The check builds the service, mocks and portal, allocates its own database, host ports and Compose project, and removes that project when finished. It runs the all-feature Rust suite, mock and portal tests, real HTTP and webhook contract checks, archived-data migration, provider reconciliation, two staff browser journeys, deck navigation, the local schema diagram and its agreement with the database, credential rotation, dependency outages and a live worker restart. A failure returns a nonzero exit code. The last message on success is `Release checks passed, including the worker restart and both browser journeys.`

For quick development feedback, `make unit` runs the default Rust, mock and portal tests with local dependencies. `make smoke` exercises a running local demo. [Engineering design](docs/design.md) · [Application contract](docs/product-verification.md) · [Migration and restore](docs/upgrade-and-reuse.md) · [Provider fixtures](mocks/README.md) · [Six reviews and implementation evidence](docs/next-iteration-review.md#development-iteration)

## Repository

| Directory | Responsibility |
| --- | --- |
| `crates/domain` | Typed observations, case transitions and pinned verification policies |
| `crates/ports`, `crates/http-transport` | Provider interfaces and bounded HTTP transport |
| `adapters/provider-*` | Each vendor’s request, response and error mapping |
| `adapters/restate`, `adapters/restate-ingress` | Durable workflows and the API-side command client |
| `adapters/postgres`, `adapters/webhook-events` | Read projections, provider limits, audit history, outbox and delivery |
| `apps/http-api`, `apps/portal` | Authenticated commands and reads, operator portal and reference |
| `config`, `mocks`, `scripts` | Local demo grants, synthetic providers and integration exercises |
