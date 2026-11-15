"""Lose A's accepted screening response, observe the B clear and recover the original hit."""
import os
import uuid
from urllib.parse import quote, urlencode

from journey_support import (API, CASES, EVENT_TOKEN, LENDING, REVIEWER, SINK, checks, delivery,
                             expect, http, issue, new_key, post, read, wait_result)
from queries_smoke import query


def inspector(vendor, op, path):
    base = os.environ[vendor.upper().replace("-", "_") + "_URL"].rstrip("/")
    return base + "/__mock__/" + path + "/" + quote(op, safe="")


def work_for(case):
    items, after = [], None
    while True:
        params = {"limit": 100}
        if after:
            params["after"] = after
        page = expect(http("GET", API + "/v2/investigations?" + urlencode(params), token=REVIEWER), 200)[1]
        items.extend(row for row in page["items"] if row["case_id"] == case)
        after = page["next_after"]
        if not after:
            return items


def assert_no_approval(case):
    history = query(CASES + "/" + case + "/history", {"limit": 100})
    assert history["revisions"] and history["next_before_version"] is None, history
    for revision in history["revisions"]:
        view = revision["view"]
        assert view["state"] not in ("APPROVED", "REJECTED"), view
        delivery(view, "action")


def main():
    subject = "strict-screen-" + uuid.uuid4().hex
    doc = issue(subject, "document", strict_screening=True, screening_scenario="held-hit")
    case = post(CASES, LENDING, {"subject_ref": subject, "use_case": "strict_screening", "evidence": {"document": doc}})
    original, replacement = f"{case}/sanctions/1", f"{case}/sanctions/2"
    a_url = inspector("sanctions-a", original, "operations")
    release = inspector("sanctions-a", original, "release")
    details = CASES + "/" + case + "/review"
    try:
        a = wait_result(a_url, "", lambda x: x.get("awaiting_release") and x["result_saved"])
        assert a["attempts"] == 1, a
        held = read(case, LENDING, lambda v: v["state"] == "PROVIDER_BLOCKED" and
                    checks(v)["sanctions"]["operation_id"] == replacement and checks(v)["sanctions"]["status"] == "PASSED")
        assert held["next_action"] == "await_original_screening", held
        b = expect(http("GET", inspector("sanctions-b", replacement, "operations")), 200)[1]
        assert b["result_saved"] and b["attempts"] == 1, b
        assigned = wait_result(details, REVIEWER, lambda v: any(h["assigned_to"] for h in v["screening_obligations"]))
        [hold] = [h for h in assigned["screening_obligations"] if h["original_operation"] == original]
        assert (hold["original_operation"],hold["replacement_operation"],hold["evidence_revision"],
                hold["assigned_to"],hold["outcome"]) == (original,replacement,doc,"screening-operations",None), hold
        early = {"check":"sanctions","disposition":"clear","operation_id":original,
                 "reason":"Trying to close an unresolved operation","review_ref":"premature"}
        expect(http("POST", details, early, token=REVIEWER, key=new_key()), 409)
        for token in [LENDING, "local-demo-routing-operator", "local-demo-institution-observer"]:
            expect(http("POST", details, early, token=token, key=new_key()), 403)
        expect(http("GET", details, token="local-demo-wallet-reviewer"), 404)
        [work] = work_for(case)
        assert work["operation_id"] == original, work
        assert_no_approval(case)
    finally:
        expect(http("POST", release, {}), 200)
    review = read(case, LENDING, lambda v: v["state"] == "AWAITING_REVIEW")
    review_case = wait_result(details, REVIEWER, lambda v: len(v["alerts"]) == 1)
    [alert] = review_case["alerts"]
    assert (alert["operation_id"],alert["provider"],alert["evidence_revision"]) == (original,"sanctions-a",doc), alert
    event = wait_result(SINK + "/events/" + quote(alert["event_id"], safe=""), EVENT_TOKEN, lambda _: True)
    assert event["operation_id"] == original and event["reason"] == "POTENTIAL_SANCTIONS_MATCH", event
    [after] = work_for(case)
    assert after["work_id"] == work["work_id"], after
    assert_no_approval(case)
    assert expect(http("GET",a_url),200)[1]["lookups"] >= 1
    post(details, REVIEWER, {"check":"sanctions","disposition":"confirm","operation_id":original,
                             "reason":"Confirmed original match in synthetic fixture","review_ref":"strict-" + case})
    rejected = read(case,LENDING,lambda v: v["state"] == "REJECTED")
    assert rejected["version"] > review["version"]
    delivery(rejected,"decided")
    print(f"Strict case {case}: A accepted, B cleared, zero approvals, original assigned, one attributable investigation, reviewer confirmed.")


def inconclusive_disposition():
    subject = "strict-inconclusive-" + uuid.uuid4().hex
    doc = issue(subject,"document",strict_screening=True,screening_scenario="inconclusive")
    case = post(CASES,LENDING,{"subject_ref":subject,"use_case":"strict_screening","evidence":{"document":doc}})
    review = read(case,LENDING,lambda v: v["state"]=="AWAITING_REVIEW" and checks(v)["sanctions"]["status"]=="PASSED")
    url = CASES + "/" + case + "/review"
    [hold] = [h for h in expect(http("GET",url,token=REVIEWER),200)[1]["screening_obligations"] if h["provider"] == "sanctions-a"]
    assert hold["outcome"] == "inconclusive" and hold["replacement_operation"] == checks(review)["sanctions"]["operation_id"], hold
    assert_no_approval(case)
    post(url,REVIEWER,{"check":"sanctions","disposition":"clear","operation_id":hold["original_operation"],
                         "reason":"Resolved inconclusive original through compliance review","review_ref":"case-file-" + case})
    terminal = read(case,LENDING,lambda v: v["state"]=="APPROVED")
    delivery(terminal,"decided")
    [decision] = expect(http("GET",url,token=REVIEWER),200)[1]["reviews"]
    assert decision["action"] == "clear" and decision["operation_id"] == hold["original_operation"] and decision["actor"] == "lending-reviewer", decision
    print(f"Strict case {case}: authoritative inconclusive result held until recorded reviewer clearance.")


def both_responses_lost():
    subject = "strict-both-lost-" + uuid.uuid4().hex
    doc = issue(subject, "document", strict_screening=True, screening_scenario="held-hit",
                replacement_scenario="held-pass")
    case = post(CASES, LENDING, {"subject_ref":subject, "use_case":"strict_screening", "evidence":{"document":doc}})
    a, b = case + "/sanctions/1", case + "/sanctions/2"
    details = CASES + "/" + case + "/review"
    release_a = inspector("sanctions-a", a, "release")
    release_b = inspector("sanctions-b", b, "release")
    try:
        for vendor, op in [("sanctions-a", a), ("sanctions-b", b)]:
            result = wait_result(inspector(vendor, op, "operations"), "",
                lambda x: x["result_saved"] and x["awaiting_release"] and x.get("lookups", 0) > 0, limit=100)
            assert result["attempts"] == 1, result
        assert_no_approval(case)
        expect(http("POST", release_b, {}), 200)
        held = read(case, LENDING, lambda v: v["state"] == "PROVIDER_BLOCKED" and
                    checks(v)["sanctions"]["operation_id"] == b and checks(v)["sanctions"]["status"] == "PASSED")
        assert held["next_action"] == "await_original_screening", held
        receipts = expect(http("GET",details,token=REVIEWER),200)[1]["screening_obligations"]
        assert {h["original_operation"]:h["outcome"] for h in receipts} == {a:None,b:"clear"}, receipts
        assert_no_approval(case)
    finally:
        # Both mock releases are idempotent. Unblock pending sockets on failure too.
        http("POST",release_b,{})
        http("POST",release_a,{})
    read(case,LENDING,lambda v:v["state"]=="AWAITING_REVIEW")
    [investigation] = work_for(case)
    assert investigation["operation_id"] == a, investigation
    post(details,REVIEWER,{"check":"sanctions","disposition":"confirm","operation_id":a,
        "reason":"Confirmed recovered original","review_ref":"both-lost-" + case})
    delivery(read(case,LENDING,lambda v:v["state"]=="REJECTED"),"decided")
    print(f"Strict case {case}: both replies lost, B clear recovered through its own lookup, A hit assigned once.")


if __name__ == "__main__":
    main()
    inconclusive_disposition()
    both_responses_lost()
