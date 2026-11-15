"""Check physical vendor receipts, unresolved address operations, mixed findings and recapture."""
from urllib.parse import quote
from address_routing_smoke import A, B, bucket, received, report, vendor, wait_for
from journey_support import CASES, WALLET, checks, issue, new_key, post, read


def open_on_a(use_case, scenario_b):
    subject = new_key()
    ref = issue(subject, "address", providers=[A, B],
                provider_scenarios={A: "lost-held-pass", B: scenario_b})
    key = new_key()
    while bucket(key, ref) >= 5:
        key = new_key()
    body = {"subject_ref": subject, "use_case": use_case, "evidence": {"address": ref}}
    return subject, post(CASES, WALLET, body, key)


def submissions(name, case):
    return [r for r in received(name, case) if r["lane"] == "submit"]


def operations(case):
    return {r["operation_id"]: r for r in report()["recent_assignments"] if r["case_id"] == case}


def assert_received_once(vendor_name, case, op):
    rows = submissions(vendor_name, case)
    assert len(rows) == 1 and rows[0]["operation_id"] == op, rows


def exercise(use_case, scenario_b):
    subject, case = open_on_a(use_case, scenario_b)
    original, replacement = case + "/address/1", case + "/address/2"
    try:
        wait_for(lambda: any(r["lane"] == "lookup" for r in received(A, case)))
        assert_received_once(A, case, original)
        if use_case == "address-replacement":
            before = read(case, WALLET, lambda v: "ADDRESS_ORIGINAL_OUTSTANDING" in checks(v)["address"]["reasons"])
            assert before["state"] == "PROVIDER_BLOCKED", before
            assert_received_once(B, case, replacement)
            rows = operations(case)
            assert rows[replacement]["predecessor"] == original and rows[replacement]["status"] == "complete", rows
            assert rows[original]["status"] == "outstanding" and rows[original]["owner"], rows
        else:
            held = read(case, WALLET, lambda v: v["state"] == "PROVIDER_BLOCKED")
            assert checks(held)["address"]["operation_id"] == original and not submissions(B, case), held
    finally:
        vendor(A, "release/" + quote(original, safe=""))
    if scenario_b == "stale" and use_case == "address-replacement":
        conflict = read(case, WALLET, lambda v: v["state"] == "AWAITING_REVIEW")
        assert checks(conflict)["address"]["reasons"] == ["CONFLICTING_ADDRESS_FINDINGS"], conflict
        assert len(operations(case)) == 2
        post(CASES + "/" + case + "/review", "local-demo-wallet-reviewer",
             {"check": "address", "operation_id": replacement, "disposition": "confirm",
              "reason": "Conflicting address findings require a fresh proof", "review_ref": new_key()})
        read(case, WALLET, lambda v: v["state"] == "AWAITING_EVIDENCE")
        fresh = issue(subject, "address", providers=[A, B], scenario="pass")
        post(CASES + "/" + case + "/evidence", WALLET, {"evidence": {"address": fresh}})
    final = read(case, WALLET, lambda v: v["state"] == "APPROVED")
    rows = operations(case)
    assert rows[original]["status"] == "complete", rows
    if use_case == "address-pilot":
        assert checks(final)["address"]["operation_id"] == original and not submissions(B, case)
    print(f"{use_case}, B={scenario_b}: original A recovered under its original ID; final state {final['state']}.", flush=True)


if __name__ == "__main__":
    exercise("address-pilot", "pass")
    exercise("address-replacement", "pass")
    exercise("address-replacement", "stale")
