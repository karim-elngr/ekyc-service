"""Exercise address routing against two independent vendors in an isolated Compose project."""

import argparse
import hashlib
import os
import time
from concurrent.futures import ThreadPoolExecutor

from http_support import API, expect, http
from journey_support import CASES, WALLET, issue, new_key, post, read
from service_contract_drills import DrillContainers

A, B = "address-a", "address-b"
OPERATOR = os.getenv("EKYC_ROUTING_OPERATOR_TOKEN", "local-demo-routing-operator")
REPORT = API.rstrip("/") + "/v2/staff/provider-routing"


def report():
    return expect(http("GET", REPORT, token=OPERATOR), 200)[1]


def vendor(name, command):
    method = "GET" if command == "ledger" else "POST"
    url = os.environ[name.upper().replace("-", "_") + "_URL"]
    return expect(
        http(method, url + "/__mock__/" + command, None if method == "GET" else {}), 200
    )[1]


def change(provider, mode, cap=None):
    prior = next(
        (row for row in report()["overrides"] if row["provider"] == provider), {}
    )
    body = dict(
        product="wallet",
        provider=provider,
        mode=mode,
        expected_version=prior.get("version", 0),
        share_cap=cap,
        duration_seconds=300,
        reason="Synthetic outage and recovery drill",
    )
    key = new_key()
    first = expect(http("PUT", REPORT, body, token=OPERATOR, key=key), 200)[1]
    assert expect(http("PUT", REPORT, body, token=OPERATOR, key=key), 200)[1] == first


def bucket(key, revision):
    case = "v2~wallet~" + hashlib.sha256(key.encode()).hexdigest()
    value = 0xCBF29CE484222325
    for byte in ("address-pilot-v1\0" + case + "/address/" + revision).encode():
        value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return value % 100


def open_case(
    target=range(100), scenario="pass", vendors=(A, B), use_case="address-pilot"
):
    subject = new_key()
    revision = issue(subject, "address", providers=list(vendors), scenario=scenario)
    key = new_key()
    while bucket(key, revision) not in target:
        key = new_key()
    body = dict(subject_ref=subject, use_case=use_case, evidence={"address": revision})
    case = post(CASES, WALLET, body, key)
    assert post(CASES, WALLET, body, key) == case
    return case


def finish(case):
    read(case, WALLET, lambda v: v["state"] == "APPROVED")
    rows = [r for r in report()["recent_assignments"] if r["case_id"] == case]
    assert rows, case
    return rows[0]


def received(name, case):
    return [
        r
        for r in vendor(name, "ledger")["requests"]
        if r["operation_id"].startswith(case + "/")
    ]


def wait_for(fn, seconds=20):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if value := fn():
            return value
        time.sleep(0.15)
    raise AssertionError("Timed out waiting for " + fn.__name__)


def run(db):
    # Stratify across all ten deciles so this small fixture has a repeatable 8:2 split.
    groups = [range(n, n + 10) for n in range(0, 100, 10)]
    with ThreadPoolExecutor(4) as pool:
        cases = list(pool.map(open_case, groups))
    for i, case in enumerate(cases):
        row = finish(case)
        chosen, other = (A, B) if i < 8 else (B, A)
        assert row["provider"] == chosen and row["bucket"] in groups[i], row
        assert len(received(chosen, case)) == 1 and not received(other, case)
    print("Weighted cohort: eight A, two B, ten vendor submissions.", flush=True)
    priority = open_case(use_case="address-priority")
    assert finish(priority)["provider"] == A
    stale = open_case(range(5), scenario="stale")
    stale_view = read(stale, WALLET, lambda v: v["state"] == "AWAITING_EVIDENCE")
    assert stale_view["checks"][0]["reasons"] == ["ADDRESS_PROOF_STALE"], stale_view
    assert len(received(A, stale)) == 1 and not received(B, stale)

    # Force real HTTP failures first. Fresh placements can observe the technical circuit.
    vendor(A, "fault/down")
    try:
        with ThreadPoolExecutor(4) as pool:
            trouble = list(pool.map(open_case, [range(4)] * 4))
        wait_for(
            lambda: db(
                "SELECT open_until IS NOT NULL FROM provider_circuit WHERE breaker_key='address-a-submit'"
            )
            == "t"
        )
        diverted = open_case(range(4))
        row = finish(diverted)
        assert row["provider"] == B and not received(A, diverted), row
        assert any(r["status"] == 503 for c in trouble for r in received(A, c))
        change(A, "draining")
        no_backup = open_case(range(4), vendors=(A,))
        # Give B exactly one more billable submission this month. The extra case must never hit its wire.
        original = int(
            db(
                "SELECT monthly_ceiling FROM provider_billing_account WHERE account='demo-address-b'"
            )
        )
        db("""UPDATE provider_billing_account SET monthly_ceiling=lookup_reserve+1+
            (SELECT coalesce(sum(cost_minor),0) FROM provider_spend_reservation
            WHERE account='demo-address-b' AND status!='released' AND period_start=date_trunc('month',now())::date)
            WHERE account='demo-address-b'""")
        try:
            allowed = open_case(range(4), use_case="address-priority")
            assert finish(allowed)["provider"] == B
            excess = open_case(range(4))
            read(excess, WALLET, lambda v: v["state"] == "PROVIDER_BLOCKED")
            assert not received(B, excess) and not received(A, excess)
            blocked = read(no_backup, WALLET, lambda v: v["state"] == "PROVIDER_BLOCKED")
            assert blocked["next_action"] == "contact_provider_operations", blocked
            assert not received(B, no_backup) and not received(A, no_backup), no_backup
            row = next(r for r in report()["recent_assignments"] if r["case_id"] == no_backup)
            assert row["provider"] == A and not row["possible_send"], row
        finally:
            db(
                f"UPDATE provider_billing_account SET monthly_ceiling={original} WHERE account='demo-address-b'"
            )
        print(
            "A returned 503. B accepted diverted traffic up to its spend ceiling. The excess case blocked without a vendor call.",
            flush=True,
        )
    finally:
        vendor(A, "fault/healthy")
        change(A, "normal")

    change(A, "recovering", 80)
    stages = []
    regressed = False
    try:
        for _ in range(62):
            case = open_case(range(5))
            row = finish(case)
            stage = next(r["stage"] for r in report()["recovery"] if r["provider"] == A)
            if not stages or stages[-1] != stage:
                stages.append(stage)
            if stages == [5, 20]:
                vendor(A, "fault/slow")
            if stages == [5, 20, 5] and not regressed:
                vendor(A, "fault/healthy")
                regressed = True
            if stage == 80:
                break
            time.sleep(0.52)
        assert stages == [5, 20, 5, 20, 50, 80], stages
        assert not received(A, diverted) and len(received(B, diverted)) == 1
    finally:
        vendor(A, "fault/healthy")
        change(A, "normal")
    returned = open_case(use_case="address-priority")
    assert finish(returned)["provider"] == A and not received(B, returned)
    print(
        "Injected high latency at 20%; observed rollback to 5%. Recovered A through measured stages 5%, 20%, 50%, 80%. B kept its accepted operation.",
        flush=True,
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--override")
    args = parser.parse_args()
    if not args.project.startswith(("ekyc-verify-", "ekyc-routing-drill")):
        parser.error("Use an isolated routing drill or verification project.")
    stack = DrillContainers(args.engine, args.project, args.override)
    pg = stack.service("postgres")["Id"]

    def db(query):
        return stack.output(
            [
                args.engine,
                "exec",
                "-i",
                pg,
                "psql",
                "-XqAt",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                "ekyc",
                "-d",
                "ekyc",
            ],
            input=query,
        ).strip()

    run(db)
