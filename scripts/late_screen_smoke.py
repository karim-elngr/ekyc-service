"""Hold a real sanctions HTTP response until a second screening has closed the loan case."""

import os
import uuid
from urllib.parse import quote, urlsplit

from journey_support import (
    CASES, EVENT_TOKEN, LENDING, REVIEWER, SINK, checks, delivery, expect, http,
    issue, new_key, open_loan, post, read, wait_result,
)


def main():
    # Reach the native mock's loopback control endpoints. All case mutations use the public API.
    origin = os.environ.get("SANCTIONS_URL", "http://127.0.0.1:9804").rstrip("/")
    if urlsplit(origin).hostname not in {"localhost", "127.0.0.1", "::1", "sanctions"}:
        raise ValueError("This fixture requires the local sanctions mock.")
    subject = "late-screen-" + uuid.uuid4().hex
    original_doc = issue(subject, "document", screening_scenario="held-hit")
    case = open_loan(subject, {
        "document": original_doc,
        "face": issue(subject, "face", document_ref=original_doc),
        "address": issue(subject, "address"),
    })
    case_url = CASES + "/" + quote(case, safe="")
    review_url = case_url + "/review"
    pending = read(case, LENDING, lambda v: checks(v)["sanctions"]["status"] == "PENDING")
    original_op = checks(pending)["sanctions"]["operation_id"]
    encoded_op = quote(original_op, safe="")
    inspector = origin + "/__mock__/operations/" + encoded_op
    release = origin + "/__mock__/release/" + encoded_op
    wait_result(inspector, "", lambda op: op.get("awaiting_release") is True, limit=30)
    try:
        new_doc = issue(subject, "document", screening_scenario="hit")
        post(case_url + "/evidence", LENDING, {
            "evidence": {"document": new_doc, "face": issue(subject, "face", document_ref=new_doc)},
        })
        current = read(case, LENDING, lambda v: v["state"] == "AWAITING_REVIEW")
        new_op = checks(current)["sanctions"]["operation_id"]
        assert original_op != new_op, current
        _, handoff = expect(http("GET", review_url, token=REVIEWER), 200)
        candidate = checks(handoff)["sanctions"]
        assert candidate["operation_id"] == new_op and candidate["evidence_revision"] == new_doc, candidate
        assert handoff["alerts"] == [], handoff
        post(review_url, REVIEWER, {
            "check": "sanctions", "operation_id": new_op, "disposition": "confirm",
            "reason": "Confirmed match for the replacement synthetic document.", "review_ref": new_key(),
        })
        terminal = read(case, LENDING, lambda v: v["state"] == "REJECTED")
        assert checks(terminal)["sanctions"]["operation_id"] == new_op
        _, held = expect(http("GET", inspector, token=""), 200)
        assert held.get("awaiting_release") and held["attempts"] == 1 and held["cache_hits"] == 0, held
    finally:
        expect(http("POST", release, {}, token=""), 200)

    alert_id = f"{case}:alert:{original_op}"
    event = wait_result(SINK.rstrip("/") + "/events/" + quote(alert_id, safe=""), EVENT_TOKEN, lambda _: True)
    assert event["event_type"] == "ekyc.screening.alert.v2" and event["event_id"] == alert_id, event
    assert (event["operation_id"], event["case_id"], event["next_action"]) == (original_op, case, "investigate"), event
    details = wait_result(review_url, REVIEWER, lambda v: bool(v["alerts"]))
    assert len(details["alerts"]) == 1, details
    alert = details["alerts"][0]
    assert (alert["event_id"], alert["operation_id"], alert["observed_at"]) == (
        alert_id, original_op, event["observed_at"]), alert
    assert (alert["provider"], alert["method"], alert["evidence_revision"], alert["evidence_ref"]) == (
        "sanctions", "sanctions_screening", original_doc, "identity-" + original_doc), alert
    assert len(details["reviews"]) == 1 and details["reviews"][0]["operation_id"] == new_op, details
    assert "alerts" not in terminal and "evidence_ref" not in event
    _, final = expect(http("GET", case_url, token=LENDING), 200)
    assert final == terminal, (terminal, final)
    delivery(terminal, "decided")
    print(f"{case}: confirmed the replacement screening; released the original hit and verified its sole alert "
          "and original evidence while the terminal decision stayed fixed.", flush=True)


def reviewed_identity_replaced():
    subject = "replaced-match-" + uuid.uuid4().hex
    original = issue(subject, "document", screening_scenario="hit")
    case = open_loan(subject, {"document": original, "face": issue(subject, "face", document_ref=original),
                               "address": issue(subject, "address")})
    pending = read(case, LENDING, lambda v: v["state"] == "AWAITING_REVIEW")
    old_operation = checks(pending)["sanctions"]["operation_id"]
    replacement = issue(subject, "document")
    case_url = CASES + "/" + case
    post(case_url + "/evidence", LENDING,
         {"evidence": {"document": replacement, "face": issue(subject, "face", document_ref=replacement)}})
    final = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    assert checks(final)["sanctions"]["operation_id"] != old_operation
    event_id = case + ":alert:" + old_operation
    event = wait_result(SINK.rstrip("/") + "/events/" + quote(event_id, safe=""), EVENT_TOKEN, lambda _: True)
    assert event["operation_id"] == old_operation and event["next_action"] == "investigate", event
    view = wait_result(case_url + "/review", REVIEWER, lambda v: bool(v["alerts"]))
    assert len(view["alerts"]) == 1 and view["alerts"][0]["evidence_revision"] == original, view
    print("Replacing an identity already awaiting review preserves its original match for investigation.", flush=True)
    return event


if __name__ == "__main__":
    main()
    reviewed_identity_replaced()
