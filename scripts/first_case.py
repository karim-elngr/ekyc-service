"""Run one synthetic loan verification through the public API and demo issuer."""

import os
import time
import uuid
from urllib.error import URLError
from urllib.parse import urljoin

from http_support import API, expect, http, wait_result


def main():
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        try:
            if http("GET", API + "/health/ready")[0] == 200:
                break
        except (URLError, OSError):
            pass
        time.sleep(0.5)
    else:
        raise RuntimeError("The API command path did not become ready within 90 seconds")
    issuer = os.getenv("EKYC_ISSUER_URL", "http://127.0.0.1:9805").rstrip("/")
    token = os.getenv("EKYC_LENDING_TOKEN", "local-demo-lending-product")
    subject = "walkthrough-" + uuid.uuid4().hex
    payload = {"subject_ref": subject, "use_case": "loan_application"}
    headers, receipt = expect(
        http("POST", API + "/v2/cases", payload, token, subject + "-open"), 202
    )
    location = urljoin(API, headers["Location"])
    print("Case opened", receipt["case_id"])
    pending = wait_result(location, token, lambda case: case["state"] == "AWAITING_EVIDENCE")
    print("Next action", pending["next_action"])

    evidence = {}
    for kind in ("document", "face", "address"):
        request = {"institution": "demo-bank", "subject_ref": subject, "kind": kind}
        if kind == "face":
            request["document_ref"] = evidence["document"]
        _, issued = expect(
            http("POST", issuer + "/demo/evidence", request, key=subject + "-" + kind), 201
        )
        evidence[kind] = issued["evidence_ref"]
    expect(
        http("POST", location + "/evidence", {"evidence": evidence}, token, subject + "-submit"),
        202,
    )
    decision = wait_result(location, token, lambda case: case["state"] in ("APPROVED", "REJECTED"))
    print("Verification", decision["state"], "under policy", decision["policy_version"])
    for check in decision["checks"]:
        print(" ", check["kind"], check["status"], check["origin"])
    assert decision["state"] == "APPROVED" and len(decision["checks"]) == 4
    assert all(
        check["status"] == "PASSED" and check["origin"] == "fresh" for check in decision["checks"]
    )


if __name__ == "__main__":
    main()
