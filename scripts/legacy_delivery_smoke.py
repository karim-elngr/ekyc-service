"""Deliver a pending v1 decision after a real API startup migrates its database."""

import argparse
import json
import time
import uuid
from urllib.parse import quote, urlsplit, urlunsplit

from http_support import wait_result
from service_contract_drills import DrillContainers, ROOT, until


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--override")
    args = parser.parse_args()
    if args.project == "ekyc":
        parser.error("Use a separate disposable Compose project.")
    stack = DrillContainers(args.engine, args.project, args.override)
    pg, api, sink = (
        stack.service(name) for name in ("postgres", "api", "webhook-sink")
    )
    network = next(iter(pg["NetworkSettings"]["Networks"]))
    pg_env = dict(item.split("=", 1) for item in pg["Config"]["Env"] if "=" in item)
    api_env = dict(item.split("=", 1) for item in api["Config"]["Env"] if "=" in item)
    user, current_db = pg_env["POSTGRES_USER"], pg_env["POSTGRES_DB"]
    tag = uuid.uuid4().hex[:12]
    scratch = "ekyc_v1_outbox_" + tag
    case_id = "legacy-" + tag
    event_id = case_id + ":2"
    payload = {
        "event_id": event_id,
        "event_type": "ekyc.case.decided.v1",
        "case_id": case_id,
        "version": 2,
        "purpose": "onboarding",
        "scope": ["document"],
        "verdict": "APPROVED",
        "policy_version": "v1",
        "decided_at": "2025-05-06T12:00:00Z",
    }
    db_url = urlsplit(api_env["DATABASE_URL"])
    scratch_url = urlunsplit(db_url._replace(path="/" + scratch))
    cli = [args.engine, "exec", "-i", pg["Id"]]

    def sql(db, statement):
        return stack.output(
            [*cli, "psql", "-XqAt", "-v", "ON_ERROR_STOP=1", "-U", user, "-d", db],
            input=statement,
        )

    stack.output([*cli, "createdb", "-U", user, scratch])
    try:
        # Copy the SQLx tracking table definition and the exact checksums from
        # the verifier's freshly migrated instance. Install only versions 1–6.
        tracker = stack.output(
            [
                *cli,
                "pg_dump",
                "-U",
                user,
                "-d",
                current_db,
                "--schema-only",
                "--no-owner",
                "--no-acl",
                "-t",
                "public._sqlx_migrations",
            ]
        )
        applied = sql(
            current_db,
            "COPY (SELECT * FROM _sqlx_migrations WHERE version<=6 ORDER BY version) TO STDOUT",
        )
        assert len(applied.splitlines()) == 6
        migrations = sorted((ROOT / "adapters/postgres/migrations").glob("*.sql"))
        bodies = [
            path.read_text() for path in migrations if int(path.name.split("_")[0]) <= 6
        ]
        sql(
            scratch,
            tracker
            + "\nSET search_path TO public;\n"
            + "\n".join(bodies)
            + "\nCOPY _sqlx_migrations FROM STDIN;\n"
            + applied
            + "\n\\.\n",
        )
        sql(
            scratch,
            f"""
            INSERT INTO case_view(case_id,version,terminal,snapshot)
                VALUES ('{case_id}',2,true,'{{"decision":{{"verdict":"APPROVED"}}}}');
            INSERT INTO case_revision(case_id,version,snapshot)
                VALUES ('{case_id}',1,'{{"pending":true}}'),('{case_id}',2,'{{"approved":true}}');
            INSERT INTO decision_outbox(event_id,case_id,version,payload,claim_count,claim_until,next_attempt_at)
                VALUES ('{event_id}','{case_id}',2,'{json.dumps(payload)}',3,
                        clock_timestamp()-interval '1 hour', clock_timestamp()-interval '2 hours');
        """,
        )
        timing = sql(
            scratch,
            "SELECT claim_until::text || '|' || next_attempt_at::text FROM decision_outbox",
        )
        api_name = "ekyc-v1-api-" + tag
        stack.start(
            api_name,
            network,
            api["Config"]["Image"],
            [],
            {
                "DATABASE_URL": scratch_url,
                "RESTATE_INGRESS_URL": api_env["RESTATE_INGRESS_URL"],
                "EKYC_CLIENTS": (ROOT / "config/demo-clients.json").read_text(),
            },
            ["ekyc-api"],
        )
        end = time.monotonic() + 35
        while (
            sql(
                scratch,
                "SELECT to_regclass('ekyc_v1_archive.case_revision') IS NOT NULL",
            )
            != "t"
        ):
            if time.monotonic() > end:
                raise AssertionError(
                    "API startup did not archive the legacy audit rows"
                )
            time.sleep(0.2)
        assert sql(scratch, "SELECT count(*) FROM ekyc_v1_archive.case_revision") == "2"
        captured = json.loads(sql(scratch, "SELECT payload FROM decision_outbox"))
        assert captured == payload
        assert (
            sql(
                scratch, "SELECT claim_count, delivered_at IS NULL FROM decision_outbox"
            )
            == "3|t"
        )
        assert (
            sql(
                scratch,
                "SELECT claim_until::text || '|' || next_attempt_at::text FROM decision_outbox",
            )
            == timing
        )

        sink_name = "ekyc-v1-inbox-" + tag
        token = "test-legacy-" + tag
        stack.start(
            sink_name,
            network,
            sink["Config"]["Image"],
            [8110],
            {
                "PORT": "8110",
                "WEBHOOK_TOKEN": token,
                "WEBHOOK_DB": "/tmp/inbox.sqlite3",
            },
            ["python3", "-u", "/sink.py"],
            ["--volume", f"{ROOT / 'mocks/webhook_sink.py'}:/sink.py:ro"],
        )
        origin = stack.origin(sink_name, 8110)
        until(origin + "/health", 200)
        stack.start(
            "ekyc-v1-relay-" + tag,
            network,
            api["Config"]["Image"],
            [],
            {
                "DATABASE_URL": scratch_url,
                "WEBHOOK_URL": f"http://{sink_name}:8110/events",
                "WEBHOOK_TOKEN": token,
            },
            ["ekyc-webhook"],
        )
        actual = wait_result(
            origin + "/events/" + quote(event_id, safe=""), token, lambda _: True
        )
        assert actual == payload
        end = time.monotonic() + 15
        while (
            sql(
                scratch,
                "SELECT delivered_at IS NOT NULL, claim_count, claim_until IS NULL FROM decision_outbox",
            )
            != "t|4|t"
        ):
            if time.monotonic() > end:
                raise AssertionError(
                    "Legacy event was not acknowledged on its new claim"
                )
            time.sleep(0.2)
        print(
            "Legacy delivery passed: two archived revisions, untouched v1 payload and prior claim, "
            "matching HTTP idempotency key at receiver, durable receipt, and acknowledged fourth claim."
        )
    finally:
        stack.clean()
        stack.output([*cli, "dropdb", "-U", user, scratch])


if __name__ == "__main__":
    main()
