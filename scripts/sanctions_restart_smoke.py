"""Crash the disposable worker after a sanctions vendor accepts and before it answers."""
import argparse
import subprocess
import time
import uuid
from pathlib import Path

from journey_support import API, LENDING, checks, issue, open_loan, operation, read, release
from reconciliation_smoke import verify_replacement
from service_contract_drills import DrillContainers, until


def wait_for_lookup(operation_id, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        counter = operation("sanctions", operation_id)
        if counter and counter.get("lookups", 0) > 0:
            return
        time.sleep(.2)
    raise AssertionError(f"Restarted worker never looked up original screening {operation_id}")


def verify(args):
    stack = DrillContainers(args.engine, args.project, args.override)
    worker = stack.service("worker")
    env = dict(item.split("=", 1) for item in worker["Config"]["Env"] if "=" in item)
    timeout = int(env["CHECK_TIMEOUT_MS"]) / 1000
    budget = int(env["CASE_DEADLINE_MS"]) / 1000
    if budget > 59:
        raise ValueError("Use the verification Compose override with a short case deadline")
    subject = "restart-screening-" + uuid.uuid4().hex
    doc = issue(subject, "document", screening_scenario="held-hit")
    evidence = {"document": doc, "face": issue(subject, "face", document_ref=doc),
                "address": issue(subject, "address")}
    started = time.monotonic()
    case = open_loan(subject, evidence)
    original = f"{case}/sanctions/1"

    def waiting_for_reply(view):
        steps = checks(view)
        pending = steps["sanctions"]
        return (pending["status"] == "PENDING" and pending["operation_id"] == original
                and all(steps[kind]["status"] == "PASSED" for kind in evidence))

    try:
        read(case, LENDING, waiting_for_reply)
        while time.monotonic() - started < timeout:
            before = operation("sanctions", original)
            if before and before.get("awaiting_release") and before["result_saved"]:
                break
            time.sleep(.025)
        else:
            raise AssertionError("Missed the first in-flight screening request")
        assert before["attempts"] == 1 and before["cache_hits"] == 0 and not before.get("lookups"), before

        # Kill by exact container ID to minimize the interval between acceptance and crash.
        # Finishing within the first timeout rules out an already-journaled HTTP error.
        subprocess.run([args.engine, "kill", "--signal", "KILL", worker["Id"]], check=True, timeout=15)
        try:
            assert time.monotonic() - started < timeout, "Worker was killed after its HTTP budget"
            print("Worker killed before the first vendor response. Waiting past the case deadline.", flush=True)
            time.sleep(budget + 1)
        finally:
            subprocess.run([args.engine, "start", worker["Id"]], check=True, timeout=30)
        until(API + "/health/ready", 200)
        blocked = read(case, LENDING, lambda view: view["state"] == "PROVIDER_BLOCKED")
        assert checks(blocked)["sanctions"]["operation_id"] == original
        wait_for_lookup(original)
        verify_replacement(subject, doc, case, original)
        final = operation("sanctions", original)
        assert final["attempts"] == 1 and final["cache_hits"] == 0 and final["lookups"] >= 1, final
        print("Recovered the original screening after worker replay past its deadline, without resubmission.")
    finally:
        counter = operation("sanctions", original)
        if counter and counter.get("awaiting_release"):
            release("sanctions", original)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--override", type=Path)
    args = parser.parse_args()
    if args.project == "ekyc":
        parser.error("Use a disposable project distinct from the regular demo")
    verify(args)
