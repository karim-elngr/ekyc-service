"""Register this local worker and wait for Restate to discover it."""
import json
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

def register(admin, worker, timeout=90):
    endpoint = admin.rstrip("/") + "/deployments"
    last_error = "waiting for Restate"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urlopen(endpoint, timeout=3) as result:
                deployments = json.load(result)["deployments"]
            if any(d.get("uri", "").rstrip("/") == worker.rstrip("/") for d in deployments):
                print("Worker already registered")
                return
            request = Request(endpoint, data=json.dumps({"uri": worker}).encode(),
                              headers={"Content-Type": "application/json"}, method="POST")
            with urlopen(request, timeout=5):
                print("Worker registered")
                return
        except HTTPError as exc:
            last_error = f"HTTP {exc.code}: {exc.read(500).decode(errors='replace')}"
        except (URLError, OSError) as exc:
            last_error = str(exc)
        time.sleep(1)
    raise SystemExit(f"Registration timed out: {last_error}")

if __name__ == "__main__":
    register(*sys.argv[1:])
