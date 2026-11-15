"""Run an isolated synthetic eKYC provider for local development."""

import argparse
import json
import os
import sqlite3
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit

from fixtures import ROUTES, SPECIFIC_FIXTURES, Reply, outcome, supports
from interactive import CASE_ID, InteractiveChecks

@dataclass
class Operation:
    evidence_ref: str
    attempts: int = 0
    terminal_reply: Reply | None = None
    cache_hits: int = 0
    lookups: int = 0
    release: threading.Event | None = None


class ProviderServer(ThreadingHTTPServer):
    def __init__(self, address, provider, timeout_delay, vendor=None):
        self.vendor = vendor
        self.fault = "healthy"
        self.provider = provider
        self.timeout_delay = timeout_delay
        self.operations = {}
        self.operation_lock = threading.Lock()
        # The production contract requires durable operation identity, including a
        # definitive negative lookup. The demo uses the provider's mounted SQLite file.
        db_path = os.environ.get("PROVIDER_DB_PATH", os.environ.get("INTERACTIVE_DB_PATH", ":memory:"))
        self.results = sqlite3.connect(db_path, check_same_thread=False)
        self.results.execute("""CREATE TABLE IF NOT EXISTS provider_results (
            id TEXT PRIMARY KEY, evidence TEXT, status INTEGER, result TEXT, held INTEGER NOT NULL)""")
        self.results.execute("""CREATE TABLE IF NOT EXISTS vendor_wire (
            id INTEGER PRIMARY KEY, operation_id TEXT NOT NULL, lane TEXT NOT NULL,
            started REAL NOT NULL, ended REAL, status INTEGER)""")
        self.interactive = None
        super().__init__(address, Handler)
        ingress = os.environ.get("RESTATE_INGRESS_URL")
        if ingress:
            db_path = os.environ.get("INTERACTIVE_DB_PATH", f"/tmp/ekyc-interactive-{provider}.sqlite3")
            self.interactive = InteractiveChecks(db_path, ingress, provider)

    def server_close(self):
        if self.interactive:
            self.interactive.close()
        super().server_close()
        self.results.close()

    def choose_response(self, operation_id, payload):
        if not isinstance(payload, dict) or set(payload) != {"response"} or not isinstance(payload["response"], str):
            return 422, {"error": "invalid_response"}
        response = payload["response"]
        prefix, _, scenario = response.partition(":")
        supported = {"pass", "review", "unavailable"} | SPECIFIC_FIXTURES[self.provider]
        if prefix != "demo" or scenario not in supported or scenario in {"held-hit", "held-pass"}:
            return 422, {"error": "unsupported_response"}
        if scenario == "unavailable":
            reply = Reply(503, {"error": "temporarily_unavailable"})
        else:
            reply = outcome(self.provider, scenario, datetime.now(timezone.utc))
        return self.interactive.choose(operation_id, response, reply)

    def evaluate(self, operation_key, evidence_ref):
        locator = evidence_ref
        if self.vendor:
            vendor_prefix = f"demo:{self.vendor}:"
            if not locator.startswith(vendor_prefix):
                return Reply(422, {"error": "wrong_vendor_evidence"}), 0.0
            locator = "demo:" + locator.removeprefix(vendor_prefix)
        prefix, separator, scenario = locator.partition(":")
        if prefix != "demo" or not separator or not supports(self.provider, scenario):
            return Reply(422, {"error": "unknown_fixture"}), 0.0

        with self.operation_lock:
            previous = self.operations.setdefault(operation_key, Operation(evidence_ref))
            row = self.results.execute("SELECT evidence,status,result,held FROM provider_results WHERE id=?",
                                       (operation_key,)).fetchone()
            if row:
                if row[0] != evidence_ref:
                    return Reply(409, {"error": "operation_key_conflict"}), 0.0
                previous.terminal_reply = Reply(row[1], json.loads(row[2]))
                if row[3] and previous.release is None:
                    previous.release = threading.Event()
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
            evaluated = datetime.now(timezone.utc)
            if scenario in {"held-hit", "held-pass", "lost-held-pass"}:
                previous.release = threading.Event()
            previous.terminal_reply = outcome(self.provider, scenario, evaluated)
            with self.results:
                self.results.execute("INSERT INTO provider_results VALUES(?,?,?,?,?)",
                    (operation_key, evidence_ref, previous.terminal_reply.status,
                     json.dumps(previous.terminal_reply.payload), int(previous.release is not None)))
            delay = self.timeout_delay if first and scenario == "timeout-once" else 0.0
            return previous.terminal_reply, delay

    def release_held(self, op):
        with self.operation_lock, self.results:
            changed = self.results.execute(
                "UPDATE provider_results SET held=0 WHERE id=? AND held=1", (op,)
            ).rowcount
            entry = self.operations.get(op)
            if entry and entry.release:
                entry.release.set()
                return True
            return bool(changed)

    def screening_result(self, op):
        with self.operation_lock, self.results:
            row = self.results.execute("SELECT status,result,held FROM provider_results WHERE id=?", (op,)).fetchone()
            if row is None:
                # Fence delayed submissions for this key before declaring it never accepted.
                self.results.execute("INSERT INTO provider_results VALUES(?,NULL,NULL,NULL,0)", (op,))
            if not row or row[0] is None:
                return Reply(200, {"status": "not_accepted"})
            pending = self.operations.get(op)
            if pending:
                pending.lookups += 1
            if row[2] and (pending is None or not pending.release or not pending.release.is_set()):
                return Reply(200, {"status": "pending"})
            if row[0] != 200:
                return Reply(200, {"status": "inconclusive"})
            return Reply(200, {"status": "complete", "result": json.loads(row[1])})


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        # Requests and request bodies can contain identity evidence references.
        pass

    def reply(self, result):
        if getattr(self, "wire", None):
            with self.server.operation_lock, self.server.results:
                self.server.results.execute("UPDATE vendor_wire SET ended=?, status=? WHERE id=?",
                                           (time.time(), result.status, self.wire))
            self.wire = None
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
        elif self.server.vendor and self.path == "/__mock__/ledger":
            with self.server.operation_lock:
                rows = self.server.results.execute("SELECT operation_id,lane,started,ended,status FROM vendor_wire ORDER BY id").fetchall()
                saved = self.server.results.execute("SELECT id FROM provider_results WHERE status IS NOT NULL ORDER BY id").fetchall()
            self.reply(Reply(200, {"requests":[dict(zip(("operation_id","lane","started","ended","status"),row)) for row in rows],
                                  "executions":[row[0] for row in saved]}))
        elif self.path.startswith("/__mock__/interactive/") and self.server.interactive:
            self.interactive_operation(self.server.interactive.read)
        elif self.path.startswith("/__mock__/operations/"):
            operation_key = unquote(self.path.removeprefix("/__mock__/operations/"))
            with self.server.operation_lock:
                operation = self.server.operations.get(operation_key)
                # Return counters for the synthetic recovery test, without exposing the payload.
                result = Reply(404, {"error": "unknown_operation"}) if operation is None else Reply(200, {
                    "attempts": operation.attempts,
                    "result_saved": operation.terminal_reply is not None,
                    "cache_hits": operation.cache_hits,
                    **({"lookups": operation.lookups} if operation.lookups else {}),
                    **({"awaiting_release": not operation.release.is_set()} if operation.release is not None else {}),
                })
            self.reply(result)
        else:
            self.reply(Reply(404, {"error": "unknown_route"}))

    def interactive_operation(self, handler, *args):
        route = urlsplit(self.path)
        case = unquote(route.path.removeprefix("/__mock__/interactive/"))
        query = parse_qs(route.query, keep_blank_values=True)
        operations = query.get("operation_id", [])
        if (not CASE_ID.fullmatch(case) or set(query) != {"operation_id"} or len(operations) != 1
                or not self.server.interactive.valid_operation(case, operations[0])):
            return self.reply(Reply(404, {"error": "unknown_operation"}))
        self.interactive_reply(handler, operations[0], *args)

    def interactive_reply(self, action, *args):
        try:
            result = Reply(*action(*args))
        except sqlite3.OperationalError:
            result = Reply(503, {"error": "mock_storage_unavailable"})
        self.reply(result)

    def do_POST(self):
        if self.path.startswith("/__mock__/release/"):
            operation_id = unquote(self.path.removeprefix("/__mock__/release/"))
            if not self.server.release_held(operation_id):
                return self.reply(Reply(404, {"error": "unknown_held_operation"}))
            return self.reply(Reply(200, {"released": True}))
        if self.server.vendor and self.path.startswith("/__mock__/fault/"):
            mode = self.path.rsplit("/", 1)[-1]
            if mode not in ("healthy", "down", "slow", "throttled"):
                return self.reply(Reply(422, {"error": "invalid_fault"}))
            self.server.fault = mode
            return self.reply(Reply(200, {"mode": mode}))
        path, input_field = ROUTES[self.server.provider]
        interactive = self.path.startswith("/__mock__/interactive/") and self.server.interactive
        lookup = (self.server.provider, self.path) in {("sanctions", "/screenings/result"), ("address", "/addresses/result")}
        if self.path != path and not interactive and not lookup:
            return self.reply(Reply(404, {"error": "unknown_route"}))
        key = self.headers.get("Idempotency-Key", "").strip()
        if not interactive and (not key or len(key) > 256):
            return self.reply(Reply(400, {"error": "invalid_operation_key"}))
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if size <= 0 or size > 4096:
                return self.reply(Reply(400, {"error": "invalid_body_size"}))
            payload = json.loads(self.rfile.read(size))
        except (ValueError, UnicodeError, RecursionError):
            return self.reply(Reply(400, {"error": "invalid_json"}))
        if self.server.vendor and not interactive:
            with self.server.operation_lock, self.server.results:
                self.wire = self.server.results.execute(
                    "INSERT INTO vendor_wire(operation_id,lane,started) VALUES(?,?,?)",
                    (key, "lookup" if lookup else "submit", time.time())).lastrowid
            if not lookup and self.server.fault == "down":
                return self.reply(Reply(503, {"error": "vendor_unavailable"}))
            if not lookup and self.server.fault == "throttled":
                return self.reply(Reply(429, {"error": "vendor_throttled"}, retry_after="1"))
            if self.server.fault == "slow":
                time.sleep(0.65)
        if lookup:
            if not isinstance(payload, dict) or payload != {"operation_id": key}:
                return self.reply(Reply(422, {"error": "invalid_lookup"}))
            return self.reply(self.server.screening_result(key))
        if interactive:
            if self.path == "/__mock__/interactive/register":
                return self.interactive_reply(interactive.register, payload)
            return self.interactive_operation(self.server.choose_response, payload)
        if not isinstance(payload, dict) or set(payload) != {input_field}:
            return self.reply(Reply(422, {"error": "invalid_provider_payload"}))
        evidence_ref = payload[input_field]
        if not isinstance(evidence_ref, str):
            return self.reply(Reply(422, {"error": "invalid_evidence_reference"}))
        result, delay = self.server.evaluate(key, evidence_ref)
        if result.status == 200 and evidence_ref.endswith(":lost-held-pass"):
            # The vendor committed the operation but the caller receives no authoritative answer.
            return self.reply(Reply(503, {"error": "response_unavailable"}))
        if result.status == 200 and evidence_ref.rsplit(":", 1)[-1] in {"held-hit", "held-pass"}:
            with self.server.operation_lock:
                release = self.server.operations[key].release
            if release and not release.wait(timeout=60):
                return self.reply(Reply(503, {"error": "mock_release_timeout"}))
        if delay:
            time.sleep(delay)
        self.reply(result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=ROUTES)
    parser.add_argument("--vendor")
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--timeout-delay", type=float, default=10.0,
                        help="Seconds before the first timeout-once reply is delivered.")
    args = parser.parse_args()
    if not 0 <= args.timeout_delay <= 600:
        parser.error("--timeout-delay must be between 0 and 600 seconds")
    with ProviderServer((args.host, args.port), args.provider, args.timeout_delay, args.vendor) as server:
        print(f"{args.provider} mock listening on port {server.server_port}", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
