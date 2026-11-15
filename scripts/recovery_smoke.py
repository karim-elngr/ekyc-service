"""Crash one explicitly supplied disposable Linux worker during an in-flight address check."""

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import time
import uuid
from urllib.error import HTTPError
from urllib.parse import quote, urlsplit
from urllib.request import urlopen

import smoke


def local_url(value):
    return urlsplit(value).hostname in {"localhost", "127.0.0.1", "::1"}


def inspect_mock(url):
    try:
        with urlopen(url, timeout=2) as response:
            return json.load(response)
    except HTTPError as err:
        with err:
            if err.code != 404:
                raise
        return {}


def sql(psql, query, case=""):
    # psql expands :'case_id' as a quoted SQL literal. Use stdin so this works
    # for both the admission probe and the terminal projection assertion.
    process = subprocess.run(
        [psql, "-XAtq", "--no-password", "--set=ON_ERROR_STOP=1",
         "--set", "case_id=" + case, "--dbname", os.environ["DATABASE_URL"]],
        input=query, text=True, capture_output=True, timeout=5, check=False,
    )
    if process.returncode:
        raise RuntimeError("PostgreSQL probe failed; check DATABASE_URL and schema")
    return process.stdout.strip()


def run(worker_fd, exe, address_url, psql):
    if not inspect_mock(address_url + "/health").get("operation_inspection"):
        raise RuntimeError("Restart the local address mock with recovery inspection enabled")
    sql(psql, "SELECT 1;")
    key = "recovery-" + uuid.uuid4().hex
    body = smoke.sample("address_refresh", address="demo:timeout-once")
    headers, admitted = smoke.expect(smoke.http("POST", smoke.API + "/v1/cases", body, key=key), 202)
    case = admitted["case_id"]
    case_url = smoke.API + headers["Location"]
    inspector = address_url + "/__mock__/operations/" + quote(case + "/address", safe="")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        first = inspect_mock(inspector)
        if first.get("result_saved"):
            if first.get("cache_hits") != 0:
                raise RuntimeError("Missed the first dispatch; worker was left running")
            in_flight = sql(psql, """
                SELECT EXISTS (SELECT FROM provider_lease
                 WHERE provider='address' AND token=:'case_id' || '/address/1'
                   AND finished_at IS NULL AND expires_at > clock_timestamp())
                 AND NOT EXISTS (SELECT FROM case_view WHERE case_id=:'case_id' AND terminal);
            """, case)
            if in_flight == "t":
                break
        time.sleep(0.025)
    else:
        raise RuntimeError("No in-flight vendor operation found; worker was left running")

    # A pidfd is tied to the original process, even if the numeric PID is reused.
    signal.pidfd_send_signal(worker_fd, signal.SIGKILL)
    poll = select.poll()
    poll.register(worker_fd, select.POLLIN)
    if not poll.poll(5000):
        raise RuntimeError("Original worker did not exit")
    replacement = subprocess.Popen([str(exe)])
    try:
        recovered = smoke.wait_result(case_url, smoke.TOKEN, lambda item: item["state"] == "DECIDED", limit=75)
        if replacement.poll() is not None or recovered["decision"] != "APPROVED":
            raise AssertionError(f"Replacement worker did not recover approval for {case}")
        final = sql(psql, """
            SELECT json_build_object(
                'view_version', (SELECT version FROM case_view WHERE case_id=:'case_id' AND terminal),
                'decision_count', (SELECT count(*) FROM case_revision WHERE case_id=:'case_id'
                    AND snapshot->>'state'='DECIDED'),
                'decision_version', (SELECT max(version) FROM case_revision WHERE case_id=:'case_id'
                    AND snapshot->>'state'='DECIDED' AND snapshot->'decision'->>'verdict'='APPROVED'),
                'event_count', (SELECT count(*) FROM decision_outbox WHERE case_id=:'case_id'),
                'event_version', (SELECT version FROM decision_outbox WHERE case_id=:'case_id'
                    AND payload->>'verdict'='APPROVED' AND event_id=case_id || ':' || version::text));
        """, case)
        receipts = json.loads(final)
        expected = dict(view_version=recovered["version"], decision_count=1,
                        decision_version=recovered["version"], event_count=1, event_version=recovered["version"])
        if receipts != expected:
            raise AssertionError(f"Read model, decision revision and event diverged: {receipts}")
        observed = inspect_mock(inspector)
        if observed.get("attempts") != 1 or observed.get("cache_hits", 0) < 1:
            raise AssertionError(f"Mock did not observe a retry of the accepted operation: {observed}")
        print(f"Worker restart recovered {case}: APPROVED v{recovered['version']}; "
              f"one vendor operation, one decision revision and one completion event.", flush=True)
    finally:
        if replacement.poll() is None:
            replacement.terminate()
            try:
                replacement.wait(timeout=5)
            except subprocess.TimeoutExpired:
                replacement.kill()
                replacement.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crash-disposable-worker", action="store_true", required=True,
                        help="Kill the specified native worker and stop its replacement after the test.")
    parser.parse_args()
    pid = int(os.environ["WORKER_PID"])
    exe = Path(os.environ["EKYC_WORKER_BIN"]).resolve(strict=True)
    if pid <= 1 or exe.name != "ekyc-worker":
        raise RuntimeError("Expected a disposable native ekyc-worker process and its binary path")
    address_url = os.environ["ADDRESS_URL"].rstrip("/")
    if not all(local_url(v) for v in (smoke.API, address_url, os.environ["DATABASE_URL"])):
        raise RuntimeError("Run only against a loopback API, address mock and PostgreSQL")
    psql = shutil.which(os.environ.get("PSQL_BIN", "psql"))
    if not psql:
        raise RuntimeError("Expose psql on PATH or set PSQL_BIN")
    worker_fd = os.pidfd_open(pid)
    try:
        proc = Path("/proc") / str(pid)
        if proc.stat().st_uid != os.getuid() or not (proc / "exe").samefile(exe):
            raise RuntimeError("WORKER_PID does not match the current user's supplied worker binary")
        environment = dict(item.split(b"=", 1) for item in (proc / "environ").read_bytes().split(b"\0") if b"=" in item)
        for name in ("DATABASE_URL", "ADDRESS_URL", "DOCUMENT_URL", "FACE_URL", "SANCTIONS_URL", "WORKER_BIND"):
            if environment.get(name.encode()) != os.environb.get(name.encode()):
                raise RuntimeError(f"{name} must match the disposable worker's configuration")
        run(worker_fd, exe, address_url, psql)
    finally:
        os.close(worker_fd)


if __name__ == "__main__":
    main()
