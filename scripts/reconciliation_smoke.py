"""Recover a vendor's original positive only through lookup after all HTTP attempts time out."""
import uuid
from urllib.parse import quote

from journey_support import (CASES, LENDING, REVIEWER, SINK, EVENT_TOKEN, checks, delivery,
                             expect, http, issue, open_loan, operation, post, read, release, set_screening, wait_result)

def main():
    subject = "reconcile-" + uuid.uuid4().hex
    doc = issue(subject, "document", screening_scenario="held-hit")
    case = open_loan(subject, {"document": doc, "face": issue(subject, "face", document_ref=doc),
                               "address": issue(subject, "address")})
    # A bounded held-response test releases the HTTP reply while a request is still alive.
    # Here the case must first exhaust *every* request and reach a replacement decision.
    blocked = read(case, LENDING, lambda v: v["state"] == "PROVIDER_BLOCKED")
    verify_replacement(subject, doc, case, checks(blocked)["sanctions"]["operation_id"])


def verify_replacement(subject, doc, case, original):
    try:
        set_screening(subject, "pass")
        post(CASES + "/" + case + "/retry", LENDING, {"check": "sanctions"})
        terminal = read(case, LENDING, lambda v: v["state"] == "APPROVED")
        assert checks(terminal)["sanctions"]["operation_id"] != original
        delivery(terminal, "decided")
        _, handoff = expect(http("GET", CASES + "/" + case + "/review", token=REVIEWER), 200)
        assert handoff["alerts"] == []
    finally:
        release("sanctions", original)
    event = wait_result(SINK + "/events/" + quote(case + ":alert:" + original, safe=""),
                        EVENT_TOKEN, lambda e: e["operation_id"] == original, limit=90)
    handoff = wait_result(CASES + "/" + case + "/review", REVIEWER, lambda v: bool(v["alerts"]))
    assert [a["event_id"] for a in handoff["alerts"]] == [event["event_id"]]
    assert handoff["alerts"][0]["evidence_revision"] == doc
    assert handoff["alerts"][0]["received_at"] == event["received_at"]
    assert event["observed_at"] < event["received_at"]
    assert operation("sanctions", original)["lookups"] >= 1
    assert expect(http("GET", CASES + "/" + case, token=LENDING), 200)[1] == terminal
    print("Original positive recovered through result lookup after timeout and replacement approval; one alert.")


if __name__ == "__main__":
    main()
