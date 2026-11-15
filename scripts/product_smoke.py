"""Exercise verification policies, evidence reuse, review and provider retries end to end."""
import uuid

from journey_support import (
    CASES, LENDING, OTHER, REVIEWER, WALLET, assert_fresh, checks, delivery, expect, http,
    issue, new_key, open_loan, open_wallet, operation, post, read, set_screening,
)


def wallet_to_loan(subject, wallet):
    loan = open_loan(subject)
    waiting = read(loan, LENDING, lambda v: v["state"] == "AWAITING_EVIDENCE"
                   and checks(v)["sanctions"]["status"] == "PASSED")
    items = checks(waiting)
    assert delivery(waiting, "action")["next_action"] == "provide_evidence"
    assert (waiting["product"], waiting["use_case"], waiting["subject_ref"]) == ("lending", "loan_application", subject)
    assert waiting["policy_version"] != wallet["policy_version"], waiting
    for kind in ("document", "face"):
        assert items[kind]["status"] == "PASSED" and items[kind]["origin"] == "reused", items[kind]
        assert items[kind]["origin_case_id"] == wallet["case_id"], items[kind]
        assert items[kind]["operation_id"] == checks(wallet)[kind]["operation_id"], items[kind]
        assert operation(kind, f"{loan}/{kind}/1") is None
    assert items["address"]["status"] == "AWAITING_EVIDENCE", items["address"]
    assert operation("address", f"{loan}/address/1") is None
    assert_fresh(loan, "sanctions", waiting)
    address = issue(subject, "address")
    body = {"evidence": {"address": address}}
    url = CASES + "/" + loan + "/evidence"
    key = new_key()
    assert post(url, LENDING, body, key) == loan
    assert post(url, LENDING, body, key) == loan
    approved = read(loan, LENDING, lambda v: v["state"] == "APPROVED")
    delivery(approved, "decided")
    assert_fresh(loan, "address", approved)
    assert_fresh(loan, "sanctions", approved)
    for kind in ("document", "face"):
        assert operation(kind, f"{loan}/{kind}/1") is None
    expect(http("GET", CASES + "/" + loan, token=WALLET), 404)
    expect(http("GET", CASES + "/" + loan, token=OTHER), 404)
    print("Wallet → loan: same subject, two policy decisions, two reused checks, one fresh screening and one address operation.", flush=True)
    return address


def tenant_isolation(subject, wallet_evidence):
    other_case = open_loan(subject, token=OTHER)
    isolated = read(other_case, OTHER, lambda v: v["state"] == "AWAITING_EVIDENCE")
    assert all(c.get("origin") != "reused" for c in isolated["checks"]), isolated
    for kind in ("face", "document"):
        assert checks(isolated)[kind]["status"] == "AWAITING_EVIDENCE", isolated
        assert operation(kind, f"{other_case}/{kind}/1") is None
    expect(http("POST", CASES + "/" + other_case + "/evidence", {"evidence": wallet_evidence},
                token=OTHER, key=new_key()), 422)
    expect(http("GET", CASES + "/" + other_case, token=LENDING), 404)
    expect(http("GET", CASES + "/" + other_case + "/review", token=REVIEWER), 404)
    print("The second institution needs its own identity evidence for the same subject reference.", flush=True)


def sanctions_review(subject, address):
    set_screening(subject, "hit")
    for disposition, final_state in (("clear", "APPROVED"), ("confirm", "REJECTED")):
        case = open_loan(subject, {"address": address})
        # A parallel address result can publish another revision after screening flags a match.
        # Wait for both checks before testing that a stale reviewer command leaves the revision intact.
        view = read(case, LENDING, lambda v: v["state"] == "AWAITING_REVIEW"
                    and checks(v)["address"]["status"] == "PASSED")
        assert delivery(view, "action")["next_action"] == "review"
        assert checks(view)["sanctions"]["status"] == "REVIEW", view
        assert_fresh(case, "sanctions", view)
        url = CASES + "/" + case + "/review"
        expect(http("GET", url, token=LENDING), 403)
        expect(http("GET", url, token=WALLET), 403)
        _, details = expect(http("GET", url, token=REVIEWER), 200)
        observed = checks(details)["sanctions"]
        assert observed["operation_id"] == checks(view)["sanctions"]["operation_id"], observed
        assert observed["evidence_ref"] and observed["evidence_revision"] and observed["observed_at"], observed
        assert observed["provider"] == "sanctions" and observed["origin_case_id"] == case, observed
        assert details["reviews"] == [], details
        payload = {"check": "sanctions", "operation_id": checks(view)["sanctions"]["operation_id"],
                   "disposition": disposition,
                   "reason": "Synthetic identity match " + disposition, "review_ref": new_key()}
        expect(http("POST", url, payload, token=LENDING, key=new_key()), 403)
        stale = {**payload, "operation_id": f"{case}/sanctions/0"}
        expect(http("POST", url, stale, token=REVIEWER, key=new_key()), 409)
        _, unchanged = expect(http("GET", CASES + "/" + case, token=LENDING), 200)
        assert unchanged["state"] == "AWAITING_REVIEW" and unchanged["version"] == view["version"], unchanged
        key = new_key()
        assert post(url, REVIEWER, payload, key) == case
        assert post(url, REVIEWER, payload, key) == case
        final = read(case, LENDING, lambda v: v["state"] == final_state)
        delivery(final, "decided")
        assert checks(final)["sanctions"]["status"] == ("PASSED" if disposition == "clear" else "FAILED"), final
        reason = "SANCTIONS_CLEARED_BY_REVIEW" if disposition == "clear" else "SANCTIONS_CONFIRMED"
        assert reason in checks(final)["sanctions"]["reasons"], final
        _, reviewed = expect(http("GET", url, token=REVIEWER), 200)
        assert len(reviewed["reviews"]) == 1, reviewed
        record = reviewed["reviews"][0]
        assert record["operation_id"] == payload["operation_id"] and record["action"] == disposition, record
        assert record["reason"] == payload["reason"] and record["review_ref"] == payload["review_ref"], record
        assert record["actor"] and record["recorded_at"], record
    print("A possible sanctions match requires a reviewer; clearing and confirming yield distinct decisions.", flush=True)


def provider_retry(subject, address):
    set_screening(subject, "unavailable")
    case = open_loan(subject, {"address": address})
    blocked = read(case, LENDING, lambda v: v["state"] == "PROVIDER_BLOCKED")
    assert delivery(blocked, "action")["next_action"] == "retry"
    item = checks(blocked)["sanctions"]
    assert item["status"] == "BLOCKED" and item["operation_id"] == f"{case}/sanctions/1", item
    old_counter = operation("sanctions", item["operation_id"])
    assert old_counter and old_counter["attempts"] >= 1 and not old_counter["result_saved"], old_counter
    set_screening(subject, "pass")
    assert post(CASES + "/" + case + "/retry", LENDING, {"check": "sanctions"}) == case
    recovered = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    delivery(recovered, "decided")
    assert_fresh(case, "sanctions", recovered, attempt=2)
    print("A sanctions outage blocks the decision; retry after recovery runs a new logical operation.", flush=True)


def correct_address(subject):
    stale = issue(subject, "address", scenario="stale")
    case = open_loan(subject, {"address": stale})
    waiting = read(case, LENDING, lambda v: "ADDRESS_PROOF_STALE" in checks(v)["address"]["reasons"])
    assert waiting["state"] == "AWAITING_EVIDENCE", waiting
    assert_fresh(case, "address", waiting)
    replacement = issue(subject, "address")
    assert replacement != stale
    url = CASES + "/" + case + "/evidence"
    assert post(url, LENDING, {"evidence": {"address": replacement}}) == case
    final = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    assert_fresh(case, "address", final, attempt=2)
    print("Stale address evidence requested a correction; a replacement approved the same loan case.", flush=True)


def replace_identity(subject, original):
    document = issue(subject, "document")
    face = issue(subject, "face", document_ref=document)
    case = open_loan(subject)
    waiting = read(case, LENDING, lambda v: v["state"] == "AWAITING_EVIDENCE")
    for kind in ("document", "face", "sanctions"):
        assert checks(waiting)[kind]["status"] == "AWAITING_EVIDENCE", waiting
        assert operation(kind, f"{case}/{kind}/1") is None
    url = CASES + "/" + case + "/evidence"
    # The prior face capture is bound to the old document portrait.
    response = http("POST", url, {"evidence": {"document": document, "face": original["face"]}},
                    token=LENDING, key=new_key())
    if response[0] not in (409, 422):
        raise AssertionError(("old portrait accepted", response))
    assert post(url, LENDING, {"evidence": {"document": document, "face": face, "address": issue(subject, "address")}}) == case
    final = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    for kind in ("document", "face", "address", "sanctions"):
        assert_fresh(case, kind, final)
    print("A replacement identity document invalidates both earlier identity checks and requires a newly bound face capture.", flush=True)


def first_time_loan():
    subject = "direct-loan-" + uuid.uuid4().hex
    case = open_loan(subject)
    waiting = read(case, LENDING, lambda v: v["state"] == "AWAITING_EVIDENCE")
    assert set(checks(waiting)) == {"document", "face", "address", "sanctions"}
    assert all(item.get("origin") != "reused" for item in waiting["checks"]), waiting
    document = issue(subject, "document")
    evidence = {"document": document, "face": issue(subject, "face"), "address": issue(subject, "address")}
    assert post(CASES + "/" + case + "/evidence", LENDING, {"evidence": evidence}) == case
    approved = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    for kind in evidence.keys() | {"sanctions"}:
        assert_fresh(case, kind, approved)
    delivery(approved, "decided")
    print("First-time applicant completed all four fresh lending checks on one loan case.", flush=True)


def full_onboarding_and_refresh():
    subject = "full-onboarding-" + uuid.uuid4().hex
    doc = issue(subject, "document")
    case = post(CASES, WALLET, {"use_case": "full_onboarding", "subject_ref": subject,
        "evidence": {"document": doc, "face": issue(subject, "face", document_ref=doc),
                     "address": issue(subject, "address")}})
    approved = read(case, WALLET, lambda v: v["state"] == "APPROVED")
    assert set(checks(approved)) == {"document", "face", "address", "sanctions"}
    for kind in checks(approved):
        assert_fresh(case, kind, approved)
    delivery(approved, "decided")
    case = post(CASES, WALLET, {"use_case": "address_refresh", "subject_ref": subject,
        "evidence": {"address": issue(subject, "address", scenario="throttle-once")}})
    refreshed = read(case, WALLET, lambda v: v["state"] == "APPROVED")
    assert set(checks(refreshed)) == {"address"}, refreshed
    counter = operation("address", checks(refreshed)["address"]["operation_id"])
    assert counter["attempts"] == 2 and counter["result_saved"], counter
    print("Full onboarding ran four checks; address refresh recovered from throttling using one operation.", flush=True)


def main():
    first_time_loan()
    full_onboarding_and_refresh()
    subject = "product-customer-" + uuid.uuid4().hex
    wallet, evidence = open_wallet(subject)
    address = wallet_to_loan(subject, wallet)
    tenant_isolation(subject, evidence)
    sanctions_review(subject, address)
    provider_retry(subject, address)
    correct_address(subject)
    replace_identity(subject, evidence)


if __name__ == "__main__":
    main()
