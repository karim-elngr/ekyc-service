"""Run an isolated synthetic eKYC provider for local development."""

import argparse
import json
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

# Each server exposes just one contract, even when all four use the same image.
ROUTES = {
    "document": ("/documents/verify", "document_ref"),
    "face": ("/faces/compare", "capture_ref"),
    "address": ("/addresses/verify", "proof_ref"),
    "sanctions": ("/screenings", "identity_ref"),
}

SHARED_FIXTURES = {
    "pass", "review", "throttle-once", "timeout-once", "unavailable", "unavailable-once",
}
SPECIFIC_FIXTURES = {
    "document": {"expired", "threshold"},
    "face": {"threshold"},
    "address": {"stale", "threshold"},
    "sanctions": {"hit", "inconclusive"},
}


@dataclass(frozen=True)
class Reply:
    status: int
    payload: dict
    retry_after: str | None = None


@dataclass
class Operation:
    evidence_ref: str
    attempts: int = 0
    terminal_reply: Reply | None = None
    cache_hits: int = 0


def outcome(provider, scenario, today):
    if provider == "document":
        days_valid = -1 if scenario == "expired" else 365
        return Reply(200, {
            "status": "REVIEW" if scenario == "review" else "PASS",
            "confidence": 85 if scenario == "threshold" else 98,
            "expires_on": (today + timedelta(days=days_valid)).isoformat(),
        })
    if provider == "face":
        return Reply(200, {
            "match_status": "REVIEW" if scenario == "review" else "PASS",
            "confidence": 95,
            "similarity_score": 85.0 if scenario == "threshold" else 96.5,
        })
    if provider == "address":
        age = 91 if scenario == "stale" else 10
        return Reply(200, {
            "status": "REVIEW" if scenario == "review" else "PASS",
            "confidence": 80 if scenario == "threshold" else 96,
            "proof_date": (today - timedelta(days=age)).isoformat(),
        })
    if scenario in {"inconclusive", "review"}:
        return Reply(422, {"error": "screening_inconclusive"})
    return Reply(200, {
        "screening_status": "HIT" if scenario == "hit" else "CLEAR",
        "match_count": 1 if scenario == "hit" else 0,
    })


class ProviderServer(ThreadingHTTPServer):
    def __init__(self, address, provider, timeout_delay):
        self.provider = provider
        self.timeout_delay = timeout_delay
        self.operations = {}
        self.operation_lock = threading.Lock()
        super().__init__(address, Handler)

    def evaluate(self, operation_key, evidence_ref):
        prefix, separator, scenario = evidence_ref.partition(":")
        valid = SHARED_FIXTURES | SPECIFIC_FIXTURES[self.provider]
        if prefix != "demo" or not separator or scenario not in valid:
            return Reply(422, {"error": "unknown_fixture"}), 0.0

        with self.operation_lock:
            previous = self.operations.setdefault(operation_key, Operation(evidence_ref))
            if previous.evidence_ref != evidence_ref:
                return Reply(409, {"error": "operation_key_conflict"}), 0.0
            if previous.terminal_reply is not None:
                previous.cache_hits += 1
                return previous.terminal_reply, 0.0
            previous.attempts += 1
            first = previous.attempts == 1
            if scenario == "throttle-once" and first:
                return Reply(429, {"error": "rate_limited"}, retry_after="1"), 0.0
            if scenario == "unavailable" or (scenario == "unavailable-once" and first):
                return Reply(503, {"error": "temporarily_unavailable"}), 0.0

            # Save the answer before simulating a lost/slow response. A retry of
            # the logical operation can retrieve it while the first socket waits.
            today = datetime.now(timezone.utc).date()
            previous.terminal_reply = outcome(self.provider, scenario, today)
            delay = self.timeout_delay if first and scenario == "timeout-once" else 0.0
            return previous.terminal_reply, delay


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        # Requests and request bodies can contain identity evidence references.
        pass

    def reply(self, result):
        data = json.dumps(result.payload, separators=(",", ":")).encode()
        try:
            self.send_response(result.status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            if result.retry_after:
                self.send_header("Retry-After", result.retry_after)
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            # Expected for the timeout fixture.
            return

    def do_GET(self):
        if self.path == "/health":
            self.reply(Reply(200, {"provider": self.server.provider, "status": "ok", "operation_inspection": True}))
        elif self.path.startswith("/__mock__/operations/"):
            operation_key = unquote(self.path.removeprefix("/__mock__/operations/"))
            with self.server.operation_lock:
                operation = self.server.operations.get(operation_key)
                # Return counters for the synthetic recovery test, without exposing the payload.
                result = Reply(404, {"error": "unknown_operation"}) if operation is None else Reply(200, {
                    "attempts": operation.attempts,
                    "result_saved": operation.terminal_reply is not None,
                    "cache_hits": operation.cache_hits,
                })
            self.reply(result)
        else:
            self.reply(Reply(404, {"error": "unknown_route"}))

    def do_POST(self):
        path, input_field = ROUTES[self.server.provider]
        if self.path != path:
            return self.reply(Reply(404, {"error": "unknown_route"}))
        key = self.headers.get("Idempotency-Key", "").strip()
        if not key or len(key) > 256:
            return self.reply(Reply(400, {"error": "invalid_operation_key"}))
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if size <= 0 or size > 4096:
                return self.reply(Reply(400, {"error": "invalid_body_size"}))
            payload = json.loads(self.rfile.read(size))
        except (ValueError, UnicodeError, RecursionError):
            return self.reply(Reply(400, {"error": "invalid_json"}))
        if not isinstance(payload, dict) or set(payload) != {input_field}:
            return self.reply(Reply(422, {"error": "invalid_provider_payload"}))
        evidence_ref = payload[input_field]
        if not isinstance(evidence_ref, str):
            return self.reply(Reply(422, {"error": "invalid_evidence_reference"}))
        result, delay = self.server.evaluate(key, evidence_ref)
        if delay:
            time.sleep(delay)
        self.reply(result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=ROUTES)
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--timeout-delay", type=float, default=10.0,
                        help="Seconds before the first timeout-once reply is delivered.")
    args = parser.parse_args()
    if not 0 <= args.timeout_delay <= 600:
        parser.error("--timeout-delay must be between 0 and 600 seconds")
    with ProviderServer((args.host, args.port), args.provider, args.timeout_delay) as server:
        print(f"{args.provider} mock listening on port {server.server_port}", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
