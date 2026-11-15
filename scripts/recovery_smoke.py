"""Verify a real worker restart during a loan provider operation."""
import argparse
import json
import time
import uuid
from pathlib import Path

from journey_support import (
    CASES, LENDING, assert_fresh, checks, delivery, expect, http, issue, new_key,
    open_loan, open_wallet, operation, post, read,
)


def prepare_recovery(path):
    """The disposable native harness owns process control and immediately restarts its worker."""
    subject = "recovery-customer-" + uuid.uuid4().hex
    wallet, _ = open_wallet(subject)
    loan = open_loan(subject)
    waiting = read(loan, LENDING, lambda v: v["state"] == "AWAITING_EVIDENCE"
                   and checks(v)["sanctions"]["status"] == "PASSED")
    delivery(waiting, "action")
    address = issue(subject, "address", scenario="timeout-once")
    payload = {"evidence": {"address": address}}
    key = new_key()
    url = CASES + "/" + loan + "/evidence"
    assert post(url, LENDING, payload, key) == loan
    path.write_text(json.dumps({"loan": loan, "wallet": wallet["case_id"], "version": waiting["version"],
                                "address": address, "key": key}))
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        counter = operation("address", f"{loan}/address/1")
        if counter and counter["result_saved"]:
            assert counter["attempts"] == 1 and not counter["cache_hits"], ("missed in-flight operation", counter)
            _, in_flight = expect(http("GET", CASES + "/" + loan, token=LENDING), 200)
            assert checks(in_flight)["address"]["status"] == "PENDING", in_flight
            print("Address provider has saved its answer; the running worker is waiting for the delayed reply.", flush=True)
            return
        time.sleep(.025)
    raise AssertionError("Address provider did not save the in-flight operation.")


def verify_recovery(path):
    saved = json.loads(path.read_text())
    case = saved["loan"]
    result = read(case, LENDING, lambda v: v["state"] == "APPROVED")
    assert result["version"] > saved["version"], result
    delivery(result, "decided")
    replay = post(CASES + "/" + case + "/evidence", LENDING,
                  {"evidence": {"address": saved["address"]}}, saved["key"])
    assert replay == case
    _, current = expect(http("GET", CASES + "/" + case, token=LENDING), 200)
    assert current == result, (current, result)
    assert_fresh(case, "address", result)
    assert_fresh(case, "sanctions", result)
    counter = operation("address", f"{case}/address/1")
    assert counter["cache_hits"] >= 1, counter
    for kind in ("face", "document"):
        item = checks(result)[kind]
        assert item["origin"] == "reused" and item["origin_case_id"] == saved["wallet"], item
        assert operation(kind, f"{case}/{kind}/1") is None
    print("Replacement worker recovered the same address operation, retained both reused checks and delivered the final decision.", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("step", choices=["prepare", "verify"])
    parser.add_argument("file", type=Path)
    args = parser.parse_args()
    if args.step == "prepare":
        prepare_recovery(args.file)
    else:
        verify_recovery(args.file)
