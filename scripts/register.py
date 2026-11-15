"""Register this local worker and wait for Restate to discover it."""
import json
import os
from pathlib import Path
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

def ensure_rules(admin):
    local = Path(__file__).resolve().parents[1] / "config/provider-contracts.json" if len(Path(__file__).resolve().parents) > 1 else Path("/config/provider-contracts.json")
    config = Path(os.environ.get("EKYC_PROVIDER_CONTRACTS_FILE", local))
    pools = json.loads(config.read_text())["pools"]
    expected = {pool + suffix: limits[field] for pool, limits in pools.items()
                for suffix, field in [("", "total"), ("/submit", "submit"), ("/lookup", "lookup")]}
    def rows():
        req = Request(admin.rstrip("/") + "/query", data=json.dumps({
            "query": "SELECT pattern, concurrency, disabled FROM sys_rules"
        }).encode(), headers={"Content-Type": "application/json", "Accept": "application/json"})
        with urlopen(req, timeout=5) as response:
            return {r["pattern"]: r for r in json.load(response)["rows"]}
    existing = rows()
    missing = [{"pattern": pattern, "limits": {"concurrency": cap},
                "precondition": {"type": "does_not_exist"}} for pattern, cap in expected.items()
               if pattern not in existing]
    if missing:
        req = Request(admin.rstrip("/") + "/limits/rules", method="PUT",
                      headers={"Content-Type": "application/json"}, data=json.dumps(missing).encode())
        with urlopen(req, timeout=5) as response:
            json.load(response)
        existing = rows()
    for pattern, cap in expected.items():
        rule = existing.get(pattern, {})
        if rule.get("disabled", True) or not 0 < (rule.get("concurrency") or 0) <= cap:
            raise RuntimeError(f"Missing or unsafe Restate concurrency rule {pattern}")

    for pool in pools:
        if existing[pool + "/submit"]["concurrency"] >= existing[pool]["concurrency"]:
            raise RuntimeError(f"Restate pool {pool} has no reserved lookup capacity")

def register(admin, worker, timeout=90):
    endpoint = admin.rstrip("/") + "/deployments"
    last_error = "waiting for Restate"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urlopen(endpoint, timeout=3) as result:
                deployments = json.load(result)["deployments"]
            matching = {item["id"] for item in deployments
                        if item.get("uri", "").rstrip("/") == worker.rstrip("/")}
            if matching:
                with urlopen(admin.rstrip("/") + "/services/ProductCase", timeout=3) as result:
                    service = json.load(result)
                handlers = {h["name"] for h in service.get("handlers", []) if h.get("public")}
                if service.get("deployment_id") in matching and {"command", "readiness_v2"} <= handlers:
                    ensure_rules(admin)
                    with urlopen(admin.rstrip("/") + "/services/ProviderAttemptV2", timeout=3) as attempts:
                        if json.load(attempts).get("deployment_id") not in matching:
                            raise SystemExit("Stale provider-attempt registration")
                    print("Current worker, attempt service and concurrency rules registered")
                    return
                raise SystemExit("Stale worker registration. Deploy this revision at a new versioned worker URI and register it.")
            ensure_rules(admin)
            request = Request(endpoint, data=json.dumps({"uri": worker}).encode(),
                              headers={"Content-Type": "application/json"}, method="POST")
            with urlopen(request, timeout=5):
                print("Worker registered, verifying its command handlers")
        except HTTPError as exc:
            last_error = f"HTTP {exc.code}: {exc.read(500).decode(errors='replace')}"
        except (URLError, OSError, RuntimeError) as exc:
            last_error = str(exc)
        time.sleep(1)
    raise SystemExit(f"Registration timed out: {last_error}")

if __name__ == "__main__":
    register(*sys.argv[1:])
