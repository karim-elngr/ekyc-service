"""Exercise the scoped worker on an isolated scheduler, database, vendor and two worker deployments."""
import argparse
import json
import os
import time
import uuid
from pathlib import Path
from urllib.parse import quote
from urllib.request import Request, urlopen

from http_support import expect, http
from journey_support import WALLET, new_key, post
from register import register
from service_contract_drills import DrillContainers, ROOT, until


def wait_for(read, condition, timeout=20):
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        last = read()
        if condition(last):
            return last
        time.sleep(.06)
    raise AssertionError(f"Capacity drill timed out: {last}")


def query(admin, statement):
    req = Request(admin + "/query", data=json.dumps({"query": statement}).encode(),
                  headers={"Content-Type": "application/json", "Accept": "application/json"})
    with urlopen(req, timeout=4) as reply:
        return json.load(reply)["rows"]


def set_cap(admin, suffix, cap, disabled=False):
    expect(http("PUT", admin + "/limits/rules", [
        {"pattern": "demo-address-b" + suffix, "limits": {"concurrency": cap}, "disabled": disabled}
    ]), 200)


def verify(args):
    stack = DrillContainers(args.engine, args.project, args.override)
    primary_worker, restate, api, pg, mock, issuer = [stack.service(s) for s in
        ("worker", "restate", "api", "postgres", "address-b", "issuer")]
    net = next(iter(primary_worker["NetworkSettings"]["Networks"]))
    prefix = "ekyc-cap-" + uuid.uuid4().hex[:8]
    db, router, vendor, registry, worker1, worker2, gateway = [prefix + "-" + s for s in
        ("db", "router", "vendor", "issuer", "worker1", "worker2", "api")]
    def environment(service):
        return dict(s.split("=", 1) for s in service["Config"]["Env"] if "=" in s)
    def image(service):
        if service is primary_worker:
            return os.environ.get("EKYC_CAPACITY_WORKER_IMAGE", service["Config"]["Image"])
        if service is mock:
            return os.environ.get("EKYC_CAPACITY_MOCK_IMAGE", service["Config"]["Image"])
        return service["Config"]["Image"]
    try:
        stack.start(db, net, image(pg), [], {"POSTGRES_USER": "ekyc", "POSTGRES_PASSWORD": "demo", "POSTGRES_DB": "drill"})
        stack.start(router, net, image(restate), [9070], {"RESTATE_ROCKSDB_TOTAL_MEMORY_SIZE": "256 MiB"})
        stack.start(vendor, net, image(mock), [8080], {"PROVIDER_DB_PATH": "/tmp/vendor.sqlite"},
                    ["address", "--vendor", "address-b"])
        issuer_env = {"PORT": "9805", "ISSUER_BIND": "0.0.0.0", "ISSUER_DB": "/tmp/issuer.sqlite",
                      "EKYC_EVIDENCE_TOKEN": "demo-capacity-token"}
        stack.start(registry, net, image(issuer), [9805], issuer_env,
                    issuer["Config"]["Cmd"], ["--volumes-from", issuer["Id"] + ":ro"])
        pg_url = "postgres://ekyc:demo@" + db + "/drill"
        worker_env = {key: value for key, value in environment(primary_worker).items()
                      if key.startswith("EKYC_") or key.endswith("_URL")}
        catalog = json.loads(worker_env["EKYC_PROVIDER_CATALOG"])
        catalog["address"]["address-b"] = "http://" + vendor + ":8080"
        worker_env.update(DATABASE_URL=pg_url, RESTATE_ADMIN_URL="http://" + router + ":9070",
                          EKYC_PROVIDER_CATALOG=json.dumps(catalog), EKYC_ROUTING_INSTITUTION="demo-bank",
                          EKYC_EVIDENCE_URL="http://" + registry + ":9805", EKYC_EVIDENCE_TOKEN="demo-capacity-token",
                          CHECK_TIMEOUT_MS="45000", CASE_DEADLINE_MS="60000")
        stack.start(worker1, net, image(primary_worker), [], worker_env, ["ekyc-worker"])
        stack.start(worker2, net, image(primary_worker), [], {**worker_env, "CASE_DEADLINE_MS": "15000",
                    "EKYC_DRILL_BEFORE_SEND_FILE":"/tmp/hold-before-http"}, ["ekyc-worker"])
        api_env = {"DATABASE_URL": pg_url, "RESTATE_INGRESS_URL": "http://" + router + ":8080",
                   "EKYC_CLIENTS": (ROOT / "config/demo-clients.json").read_text()}
        stack.start(gateway, net, image(api), [8090], api_env, ["ekyc-api"])
        admin, mock_url, issuer_url, origin = (stack.origin(name, port) for name, port in
                                                [(router,9070),(vendor,8080),(registry,9805),(gateway,8090)])
        register(admin, "http://" + worker1 + ":9080")
        print("First capacity worker registered", flush=True)
        until(origin + "/health/ready", 200)
        def open_case(scenario):
            subject = "capacity-" + new_key()
            _, asset = expect(http("POST", issuer_url + "/demo/evidence", {
                "institution":"demo-bank", "subject_ref":subject, "kind":"address",
                "scenario":scenario, "providers":["address-b"]}, key=new_key()), 201)
            return post(origin + "/v2/cases", WALLET, {"subject_ref":subject, "use_case":"address-pilot",
                                     "evidence":{"address":asset["evidence_ref"]}})
        def ledger():
            return expect(http("GET", mock_url + "/__mock__/ledger"), 200)[1]["requests"]
        def issued(case, lane="submit"):
            return wait_for(ledger, lambda rows: any(r["operation_id"].startswith(case + "/") and r["lane"] == lane for r in rows))
        def release(case):
            row = next(r for r in ledger() if r["lane"] == "submit" and r["operation_id"].startswith(case + "/"))
            expect(http("POST", mock_url + "/__mock__/release/" + quote(row["operation_id"], safe=""), {}), 200)
        def read_case(case, state):
            return wait_for(lambda: expect(http("GET", origin + "/v2/cases/" + case, token=WALLET),200)[1],
                            lambda item: item["state"] == state, 40)
        lookup = open_case("lost-held-pass")
        issued(lookup, "lookup")
        print("Vendor confirmed an outstanding operation and its first lookup", flush=True)
        first = [open_case("held-pass") for _ in range(2)]
        for case in first:
            issued(case)
        # Calls already running remain on worker1. A new deployment handles the next calls.
        register(admin, "http://" + worker2 + ":9080")
        print("Second capacity worker registered with two earlier vendor requests still active", flush=True)
        third = open_case("held-pass")
        issued(third)
        queued = open_case("pass")
        pool = "SELECT rule_pattern,usage,num_waiters FROM sys_user_limits WHERE scope='demo-address-b'"
        wait_for(lambda: query(admin, pool), lambda rows: any(
            r.get("rule_pattern")=="demo-address-b/submit" and r["usage"]==3 and r["num_waiters"]>=1 for r in rows))
        deployments = query(admin, "SELECT DISTINCT pinned_deployment_id FROM sys_invocation WHERE "
                                   "target_service_name='ProviderAttemptV2' AND scope='demo-address-b' "
                                   "AND limit_key='submit' AND status='running'")
        assert len(deployments) == 2, deployments
        print("Restate reports three admitted submissions and a waiting submission across two deployments", flush=True)
        expect(http("POST", mock_url + "/__mock__/fault/slow", {}), 200)
        baseline = len(ledger())
        snapshot = wait_for(ledger, lambda rows: any(r["lane"]=="lookup" and r["ended"] is not None
                                                    for r in rows[baseline:]))
        check_time = next(r["started"] for r in snapshot[baseline:] if r["lane"]=="lookup")
        active = [r for r in snapshot if r["started"]<=check_time and (r["ended"] is None or r["ended"]>check_time)]
        assert sorted(r["lane"] for r in active)==["lookup","submit","submit","submit"], active
        assert not any(r["operation_id"].startswith(queued + "/") for r in snapshot)
        # A live lower cap holds new work until old calls finish. Keep the new job queued past its own deadline.
        set_cap(admin, "/submit", 2)
        release(third)
        read_case(queued, "PROVIDER_BLOCKED")
        for case in first:
            release(case)
        release(lookup)
        for case in [*first, third, lookup]:
            read_case(case, "APPROVED")
        wait_for(lambda: query(admin, "SELECT status FROM sys_invocation WHERE target_service_name='ProviderAttemptV2' "
                                  "AND scope='demo-address-b' AND limit_key='submit'"), lambda rows: all(
                                      r["status"]=="completed" for r in rows))
        rows = ledger()
        assert not any(r["operation_id"].startswith(queued + "/") for r in rows), rows
        # The vendor owns start/end timestamps. Count overlapping requests over the entire cohort.
        events = sorted((instant, delta, row["lane"]) for row in rows
                        for instant, delta in [(row["started"], 1), (row["ended"], -1)]
                        if instant is not None)
        active = {"submit": 0, "lookup": 0}
        peak = {"submit": 0, "lookup": 0, "total": 0}
        for _, delta, lane in events:
            active[lane] += delta
            peak[lane] = max(peak[lane], active[lane])
            peak["total"] = max(peak["total"], sum(active.values()))
        assert peak == {"submit": 3, "lookup": 1, "total": 4}, peak
        print("Independent vendor peak", peak, flush=True)
        expect(http("POST", mock_url + "/__mock__/fault/healthy", {}), 200)
        # A required rule disappears. The read readiness endpoint remains usable; commands require healthy rules.
        before = len(rows)
        set_cap(admin, "/submit", 2, disabled=True)
        until(origin + "/health/ready", 503)
        waiting = open_case("pass")
        wait_for(lambda: query(admin, "SELECT status FROM sys_invocation WHERE target_service_name='ProviderAttemptV2' "
                                  "AND idempotency_key LIKE '" + waiting + "/%'"),
                 lambda rows: any(r["status"]=="completed" for r in rows))
        assert len(ledger())==before
        set_cap(admin, "/submit", 3)
        until(origin + "/health/ready", 200)
        read_case(waiting, "APPROVED")
        def sql(statement):
            return stack.output([args.engine,"exec",db,"psql","-At","-U","ekyc","-d","drill","-c",statement])
        for phase in ("before_http","after_accept"):
            if phase == "before_http":
                stack.output([args.engine,"exec",worker2,"touch","/tmp/hold-before-http"])
            case = open_case("pass" if phase=="before_http" else "held-pass")
            # Case IDs are generated hashes from the authenticated API.
            rows = lambda: [r for r in ledger() if r["operation_id"].startswith(case + "/")]
            if phase == "after_accept":
                issued(case)
            wait_for(lambda: sql("SELECT status FROM provider_route_selection WHERE case_id='" + case + "'"),
                     lambda status: status=="outstanding")
            stack.output([args.engine,"kill","--signal","KILL", worker2])
            starts = [r for r in rows() if r["lane"]=="submit"]
            assert len(starts)==int(phase=="after_accept"), (phase,starts)
            assert sql("SELECT class FROM provider_attempt WHERE case_id='" + case + "'")=="possible_send"
            stack.output([args.engine,"start",worker2])
            until(origin + "/health/ready",200)
            if phase == "before_http":
                stack.output([args.engine,"exec",worker2,"rm","/tmp/hold-before-http"])
            else:
                release(case)
            issued(case,"lookup")
            wait_for(lambda: sql("SELECT status FROM provider_route_selection WHERE case_id='" + case + "'"),
                     lambda status: status==("not_accepted" if phase=="before_http" else "complete"), 35)
            assert len([r for r in rows() if r["lane"]=="submit"])==len(starts)
            assert sql("SELECT count(*) FROM provider_attempt WHERE lane='submit' AND case_id='" + case + "'")=="1"
        expect(http("POST",mock_url + "/__mock__/fault/throttled",{}),200)
        throttled = open_case("pass")
        issued(throttled,"lookup")
        wait_for(lambda: sql("SELECT status FROM provider_route_selection WHERE case_id='" + throttled + "'"),
                 lambda status: status=="not_accepted")
        charges = sql("SELECT status FROM provider_spend_reservation WHERE lane='submit' AND operation_id "
                      "IN (SELECT operation_id FROM provider_route_selection WHERE case_id='" + throttled + "')")
        assert charges=="released", charges
        expect(http("POST",mock_url + "/__mock__/fault/healthy",{}),200)
        print("Both worker kills recovered via result-only lookup: zero starts before HTTP and one accepted "
              "operation at the vendor before journaling. Neither consumed authorization sent again.")
        print("Two worker deployments held 3 B submissions; an independent B lookup used slot 4. "
              "Restate recorded the queued fourth submission; it expired with zero vendor starts. "
              "Lowered and disabled rules were observed and fresh work resumed.")
    except Exception:
        if "admin" in locals():
            # Preserve scheduler evidence before disposing of this isolated cluster.
            for statement in [
                "SELECT rule_pattern,usage,num_waiters FROM sys_user_limits WHERE scope='demo-address-b'",
                "SELECT status,limit_key,pinned_deployment_id,last_failure FROM sys_invocation WHERE target_service_name='ProviderAttemptV2' LIMIT 25",
            ]:
                try:
                    print("Capacity diagnostic", query(admin, statement), flush=True)
                except OSError:
                    pass
        raise
    finally:
        stack.clean()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--override", type=Path)
    verify(parser.parse_args())
