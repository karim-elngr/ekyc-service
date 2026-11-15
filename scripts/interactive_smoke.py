"""Drive the v2 provider callbacks through the case API and verify the decision webhook."""
import uuid
from urllib.parse import urlencode

from journey_support import CASES, WALLET, checks, delivery, expect, http, issue, post, read, wait_result


def main():
    subject = "live-provider-" + uuid.uuid4().hex
    address = issue(subject, "address", scenario="interactive")
    case = post(CASES, WALLET, {"subject_ref": subject, "use_case": "address_refresh",
                                "evidence": {"address": address}})
    case_url = CASES + "/" + case
    lab = case_url + "/interactive"
    live = wait_result(lab, WALLET, lambda v: any(c["status"] == "awaiting_response" for c in v["checks"]))
    registered = checks(live)["address"]
    assert registered["invocation_id"].startswith("inv_"), live
    operation = case + "/address/1"
    url = lab + "/address?" + urlencode({"operation_id": operation})
    expect(http("POST", lab + "/address?" + urlencode({"operation_id": case + "/address/2"}),
                {"response": "demo:pass"}, token=WALLET), 409)
    status, _, body = http("POST", url, {"response": "demo:pass"}, token=WALLET)
    assert status in (200, 202), (status, body)
    passed = read(case, WALLET, lambda v: v["state"] == "APPROVED")
    assert set(checks(passed)) == {"address"} and checks(passed)["address"]["operation_id"] == operation
    delivery(passed, "decided")
    print("Live address callback selected its exact pending operation, resumed its workflow, and delivered approval.")


if __name__ == "__main__":
    main()
