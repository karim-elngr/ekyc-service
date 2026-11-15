"""Change an issuer method while a real, registered provider operation is pending."""
from urllib.parse import urlencode

from journey_support import (
    CASES, ISSUER, LENDING, REVIEWER, checks, expect, http, issue, new_key, open_loan,
    post, read, set_screening, wait_result,
)


def main():
    subject = "binding-drift-" + new_key()
    doc = issue(subject, "document", screening_scenario="interactive")
    case = open_loan(subject, {"document": doc, "face": issue(subject, "face", document_ref=doc),
                               "address": issue(subject, "address")})
    url = CASES + "/" + case
    read(case, LENDING, lambda v: all(checks(v)[k]["status"] == "PASSED"
                                     for k in ("document", "face", "address")))
    active = wait_result(url + "/interactive", LENDING,
                         lambda v: checks(v).get("sanctions", {}).get("status") == "awaiting_response")
    op = checks(active)["sanctions"]["operation_id"]
    expect(http("POST", ISSUER + "/demo/screening",
                {"institution":"demo-bank", "subject_ref":subject, "scenario":"interactive",
                 "method":"renamed_screen"}, token=""), 200)
    status, _, body = http("POST", url + "/interactive/sanctions?" + urlencode({"operation_id":op}),
                           {"response":"demo:hit"}, token=LENDING)
    assert status in (200, 202), (status, body)
    blocked = read(case, LENDING, lambda v: v["state"] == "PROVIDER_BLOCKED")
    assert checks(blocked)["sanctions"]["reasons"] == ["INVALID_EVIDENCE_BINDING"], blocked
    handoff = wait_result(url + "/review", REVIEWER, lambda v: bool(v["alerts"]))
    assert len(handoff["alerts"]) == 1 and handoff["alerts"][0]["operation_id"] == op, handoff
    set_screening(subject, "pass")
    post(url + "/retry", LENDING, {"check":"sanctions"})
    final = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    assert checks(final)["sanctions"]["operation_id"] != op, final
    _, handoff = expect(http("GET", url + "/review", token=REVIEWER), 200)
    assert len(handoff["alerts"]) == 1, handoff
    print("Changed issuer method blocked the check, preserved its positive alert and recovered on a fresh operation.")


if __name__ == "__main__":
    main()
