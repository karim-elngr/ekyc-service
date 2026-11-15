"""Retrieve a cached wallet pass after a newer adverse evaluation, then check lending eligibility."""
from datetime import datetime
import time
import uuid

from journey_support import CASES, LENDING, WALLET, checks, issue, open_loan, operation, post, read, release


def wait_counter(kind, op, predicate, limit=55):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        data = operation(kind, op)
        if data and predicate(data):
            return data
        time.sleep(.1)
    raise AssertionError(f"Timed out waiting for {op} at {kind}")


def main():
    subject = "evaluation-order-" + uuid.uuid4().hex
    doc = issue(subject, "document")

    def wallet(scenario):
        face = issue(subject, "face", document_ref=doc, scenario=scenario)
        return post(CASES, WALLET, {"subject_ref":subject, "use_case":"onboarding",
                                    "evidence":{"document":doc,"face":face}})

    older = wallet("held-pass")
    op = older + "/face/1"
    wait_counter("face", op, lambda v: v.get("awaiting_release"))
    try:
        recent = wallet("review")
        adverse = read(recent, WALLET, lambda v: "FACE_NOT_VERIFIED" in checks(v)["face"]["reasons"])
        wait_counter("face", op, lambda v: v["cache_hits"] > 0)
    finally:
        release("face", op)
    old_case = read(older, WALLET, lambda v: v["state"]=="APPROVED")
    old, newer = checks(old_case)["face"], checks(adverse)["face"]
    def stamp(value):
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    assert stamp(old["observed_at"]) < stamp(newer["observed_at"]) <= stamp(old["received_at"]), (old,newer)
    loan = open_loan(subject)
    lending = read(loan, LENDING, lambda v: checks(v)["face"]["status"] == "AWAITING_EVIDENCE")
    assert checks(lending)["face"]["origin"] is None
    print("Lending requested fresh face evidence after a cached older pass arrived after a newer adverse evaluation.")


if __name__ == "__main__":
    main()
