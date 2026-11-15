"""Fixtures and assertions shared by the end-to-end verification journeys."""

import json
import os
import shutil
import subprocess
import uuid
from concurrent.futures import ThreadPoolExecutor
from urllib.error import URLError
from urllib.parse import quote

from http_support import API, EVENT_TOKEN, SINK, expect, http, wait_result


ISSUER = os.environ.get("EKYC_ISSUER_URL", "http://127.0.0.1:9805").rstrip("/")
CASES = API.rstrip("/") + "/v2/cases"
WALLET = os.environ.get("EKYC_WALLET_TOKEN", "local-demo-wallet-product")
LENDING = os.environ.get("EKYC_LENDING_TOKEN", "local-demo-lending-product")
OTHER = os.environ.get("EKYC_OTHER_LENDING_TOKEN", "local-demo-other-lending")
REVIEWER = os.environ.get("EKYC_LENDING_REVIEWER_TOKEN", "local-demo-lending-reviewer")
PORT = {"document": 9801, "face": 9802, "address": 9803, "sanctions": 9804}


def new_key():
    return "v2-" + uuid.uuid4().hex


def issue(subject, kind, **options):
    payload = {"institution": "demo-bank", "subject_ref": subject, "kind": kind, **options}
    _, result = expect(http("POST", ISSUER + "/demo/evidence", payload, token="", key=new_key()), 201)
    return result["evidence_ref"]


def set_screening(subject, scenario):
    expect(http("POST", ISSUER + "/demo/screening", {
        "institution": "demo-bank", "subject_ref": subject, "scenario": scenario,
    }, token="", key=new_key()), 200)


def post(url, token, body, key=None):
    _, result = expect(http("POST", url, body, token=token, key=key or new_key()), 202)
    return result["case_id"]


def read(case, token, predicate):
    return wait_result(CASES + "/" + quote(case, safe=""), token, predicate, limit=65)


def checks(view):
    return {item["kind"]: item for item in view["checks"]}


def delivery(view, kind):
    event_id = f"{view['case_id']}:{view['version']}"
    event = wait_result(SINK.rstrip("/") + "/events/" + quote(event_id, safe=""),
                        EVENT_TOKEN, lambda _: True)
    assert event["event_id"] == event_id and event["event_type"] == "ekyc.case." + kind + ".v2", event
    for key in ("case_id", "version", "policy_version", "product", "use_case", "state", "next_action", "checks"):
        assert event[key] == view[key], (key, event, view)
    assert event["institution"] == "demo-bank", event
    return event


def operation(kind, operation_id):
    """Read actual provider-side counters over loopback or the private Compose network."""
    path = "/__mock__/operations/" + quote(operation_id, safe="")
    override = os.environ.get(kind.upper() + "_URL")
    origin = override or f"http://127.0.0.1:{PORT[kind]}"
    try:
        status, _, data = http("GET", origin.rstrip("/") + path, token="")
    except URLError:
        if override:
            raise
        engine = shutil.which("docker") or shutil.which("podman")
        if not engine:
            raise RuntimeError("Set " + kind.upper() + "_URL to reach the mock inspector.")
        # Compose's provider ports are private. Read the same counter from inside its container.
        probe = ("import http.client,json,sys; c=http.client.HTTPConnection('127.0.0.1',8080,timeout=5); "
                 "c.request('GET',sys.argv[1]); r=c.getresponse(); print(json.dumps([r.status,json.load(r)]))")
        output = subprocess.run([engine, "compose", "exec", "-T", kind, "python3", "-c", probe, path],
                                text=True, capture_output=True, timeout=15, check=True)
        status, data = json.loads(output.stdout)
    if status == 404 and data.get("error") == "unknown_operation":
        return None
    if status != 200 or "attempts" not in data:
        raise AssertionError((kind, operation_id, status, data))
    return data


def release(kind, operation_id):
    path = "/__mock__/release/" + quote(operation_id, safe="")
    try:
        expect(http("POST", os.environ.get(kind.upper() + "_URL", f"http://127.0.0.1:{PORT[kind]}").rstrip("/") + path, {}), 200)
    except URLError:
        if kind.upper() + "_URL" in os.environ:
            raise
        engine = shutil.which("docker") or shutil.which("podman")
        probe = ("import http.client,sys; c=http.client.HTTPConnection('127.0.0.1',8080); "
                 "c.request('POST',sys.argv[1],b'{}'); assert c.getresponse().status==200")
        subprocess.run([engine, "compose", "exec", "-T", kind, "python3", "-c", probe, path], check=True)



def assert_fresh(case, kind, view, attempt=1):
    check = checks(view)[kind]
    expected_id = f"{case}/{kind}/{attempt}"
    assert check["origin"] == "fresh" and check["operation_id"] == expected_id, check
    counter = operation(kind, expected_id)
    assert counter and counter["result_saved"] and counter["attempts"] == 1, (kind, counter)


def open_wallet(subject):
    doc = issue(subject, "document")
    face = issue(subject, "face", document_ref=doc)
    body = {"use_case": "onboarding", "subject_ref": subject, "evidence": {"document": doc, "face": face}}
    key = new_key()
    with ThreadPoolExecutor(max_workers=4) as pool:
        ids = list(pool.map(lambda _: post(CASES, WALLET, body, key), range(4)))
    assert len(set(ids)) == 1, ids
    case = ids[0]
    wallet = read(case, WALLET, lambda v: v["state"] == "APPROVED")
    delivery(wallet, "decided")
    assert (wallet["product"], wallet["use_case"], wallet["subject_ref"]) == ("wallet", "onboarding", subject), wallet
    for kind in ("face", "document", "sanctions"):
        assert_fresh(case, kind, wallet)
    expect(http("POST", CASES, {**body, "subject_ref": subject + "-different"}, token=WALLET, key=key), 409)
    expect(http("GET", CASES + "/" + case, token=LENDING), 404)
    expect(http("POST", CASES, body, token=LENDING, key=new_key()), 403)
    return wallet, body["evidence"]


def open_loan(subject, evidence=None, token=LENDING):
    body = {"use_case": "loan_application", "subject_ref": subject}
    if evidence:
        body["evidence"] = evidence
    return post(CASES, token, body)
