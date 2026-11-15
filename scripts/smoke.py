"""Exercise the running API, Restate handlers, databases, providers and event sink."""
import json
import os
import time
import uuid
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

API = os.environ.get("EKYC_API_URL", "http://127.0.0.1:8090")
SINK = os.environ.get("EKYC_SINK_URL", "http://127.0.0.1:8110")
TOKEN = os.environ.get("EKYC_DEMO_TOKEN", "local-demo-onboarding")
PARTNER = os.environ.get("EKYC_PARTNER_TOKEN", "local-demo-refresher")
EVENT_TOKEN = os.environ.get("WEBHOOK_TOKEN", "local-demo-events-token")

def http(method, url, data=None, token=TOKEN, key=None):
    headers = {"Authorization": "Bearer " + token}
    if key:
        headers["Idempotency-Key"] = key
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = Request(url, data=None if data is None else json.dumps(data).encode(),
                  headers=headers, method=method)
    try:
        reply = urlopen(req, timeout=12)
    except HTTPError as err:
        reply = err
    with reply:
        raw = reply.read()
        content_type = reply.headers.get("Content-Type", "")
        val = json.loads(raw) if raw and "json" in content_type else raw.decode()
        return reply.status, reply.headers, val

def expect(result, code):
    status, headers, body = result
    if status != code:
        raise AssertionError(f"expected HTTP {code}, got {status}: {body}")
    return headers, body

def wait_result(url, token, predicate, limit=50):
    end = time.monotonic() + limit
    last = None
    while time.monotonic() < end:
        status, _, last = http("GET", url, token=token)
        if status == 200 and predicate(last):
            return last
        if status not in (200, 404, 503):
            raise AssertionError((status, last))
        time.sleep(.2)
    raise AssertionError(f"timed out waiting for {url}: {last}")

def sample(purpose="full_onboarding", **overrides):
    kinds = ("document", "face", "address", "sanctions") if purpose == "full_onboarding" else ("address",)
    evidence = {key: "demo:pass" for key in kinds}
    evidence.update(overrides)
    return {"subject_ref": "synthetic-customer", "purpose": purpose, "evidence": evidence}

def scenario(name, body, expected, reason=None):
    key = "smoke-" + uuid.uuid4().hex
    headers, admitted = expect(http("POST", API + "/v1/cases", body, key=key), 202)
    url = API + headers["Location"]
    result = wait_result(url, TOKEN, lambda item: item["state"] == "DECIDED")
    assert result["case_id"] == admitted["case_id"]
    assert result["decision"] == expected, result
    assert result["policy_version"] == "v1"
    if reason:
        assert reason in [r["code"] for r in result["reasons"]], result
    assert len(result["scope"]) == (1 if body["purpose"] == "address_refresh" else 4)
    event_id = f'{result["case_id"]}:{result["version"]}'
    event = wait_result(SINK + "/events/" + quote(event_id, safe=""), EVENT_TOKEN, lambda _: True)
    assert event["event_id"] == event_id and event["case_id"] == result["case_id"], event
    print(f"{name} -> {expected}", flush=True)
    return key, result

def admission_limit():
    # Repeat a single scoped command to avoid generating unrelated vendor jobs.
    token = os.environ.get("EKYC_QUOTA_TEST_TOKEN", "local-demo-quota-test")
    key = "quota-" + uuid.uuid4().hex
    for _ in range(int(os.environ.get("ADMISSION_PER_MINUTE", "60")) * 2 + 2):
        status, headers, body = http("POST", API + "/v1/cases", sample("address_refresh"), token=token, key=key)
        if status == 429:
            assert 1 <= int(headers["Retry-After"]) <= 60
            print("caller quota -> 429 with Retry-After", flush=True)
            return
        if status != 202:
            raise AssertionError((status, body))
    raise AssertionError("configured per-caller quota did not take effect")

def main():
    expect(http("GET", API + "/health/ready"), 200)
    body = sample()
    key, full = scenario("full onboarding", body, "APPROVED")
    _, same = expect(http("POST", API + "/v1/cases", body, key=key), 202)
    assert same["case_id"] == full["case_id"]
    expect(http("POST", API + "/v1/cases", sample("address_refresh"), key=key), 409)
    expect(http("GET", API + "/v1/cases/" + full["case_id"], token=PARTNER), 404)
    expect(http("POST", API + "/v1/cases", body, token=PARTNER, key=key), 403)
    expect(http("POST", API + "/v1/cases", body, token="unknown", key=key), 401)
    scenario("low similarity", sample(face="demo:threshold"), "MANUAL_REVIEW", "FACE_SIMILARITY_LOW")
    scenario("expired ID", sample(document="demo:expired"), "REJECTED", "DOCUMENT_EXPIRED")
    scenario("sanctions hit", sample(sanctions="demo:hit"), "REJECTED", "SANCTIONS_HIT")
    scenario("recover from throttle", sample("address_refresh", address="demo:throttle-once"), "APPROVED")
    scenario("recover after vendor accepted a timed-out request", sample("address_refresh", address="demo:timeout-once"), "APPROVED")
    scenario("exhausted sanctions outage", sample(sanctions="demo:unavailable"), "REJECTED", "SANCTIONS_UNAVAILABLE")
    admission_limit()
    print("All API and webhook checks passed", flush=True)

if __name__ == "__main__":
    main()
