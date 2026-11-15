"""Exercise the running API, Restate handlers, databases, providers and event sink."""
import json
import os
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

API = os.environ.get("EKYC_API_URL", "http://127.0.0.1:8090")
SINK = os.environ.get("EKYC_SINK_URL", "http://127.0.0.1:8110")
EVENT_TOKEN = os.environ.get("WEBHOOK_TOKEN", "local-demo-events-token")

def http(method, url, data=None, token="", key=None):
    headers = {"Authorization": "Bearer " + token} if token else {}
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
