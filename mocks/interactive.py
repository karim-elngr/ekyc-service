"""Persistent, presenter-controlled responses for one demo provider."""

import http.client
import json
import re
import sqlite3
import threading
import time
from contextlib import closing, contextmanager
from urllib.parse import urlsplit


CASE_ID = re.compile(r"v2~[A-Za-z0-9-]{1,48}~[0-9a-f]{64}")
AWAKEABLE_ID = re.compile(r"(?:sign|prom)_1[A-Za-z0-9_-]{1,180}")
INVOCATION_ID = re.compile(r"inv_[A-Za-z0-9_-]{1,180}")


def status(row):
    if row is None:
        return 404, {"error": "check_not_registered"}
    if row["sent"]:
        state = "sent"
    elif row["response"] is None:
        state = "awaiting_response"
    else:
        state = "sending"
    result = {"status": state, "response": row["response"]}
    if row["invocation_id"]:
        result["invocation_id"] = row["invocation_id"]
    return 200, result


class InteractiveChecks:
    def __init__(self, db_path, ingress, provider):
        self.provider = provider
        self.db_path = db_path
        self.ingress = urlsplit(ingress)
        if (self.ingress.scheme not in {"http", "https"} or not self.ingress.hostname
                or self.ingress.username or self.ingress.query or self.ingress.fragment):
            raise ValueError("invalid Restate ingress URL")
        with self.connect() as db:
            db.execute("DROP TABLE IF EXISTS interactive_checks")  # Retired case-keyed demo registrations.
            db.execute("""
                CREATE TABLE IF NOT EXISTS callbacks (
                  operation_id TEXT PRIMARY KEY, awakeable_id TEXT NOT NULL UNIQUE, invocation_id TEXT,
                  response TEXT, callback TEXT, sent INTEGER NOT NULL DEFAULT 0,
                  failures INTEGER NOT NULL DEFAULT 0, next_attempt REAL NOT NULL DEFAULT 0
                )
            """)
            db.execute("""
                CREATE INDEX IF NOT EXISTS due_callbacks ON callbacks(next_attempt)
                WHERE callback IS NOT NULL AND sent = 0
            """)
        self.changed = threading.Event()
        self.stopped = threading.Event()
        self.worker = threading.Thread(target=self.deliver, name="mock-callbacks", daemon=True)
        self.worker.start()

    @contextmanager
    def connect(self):
        with closing(sqlite3.connect(self.db_path)) as db, db:
            db.row_factory = sqlite3.Row
            yield db

    def close(self):
        self.stopped.set()
        self.changed.set()
        self.worker.join()

    def valid_operation(self, case, operation):
        if not isinstance(operation, str):
            return False
        components = operation.split("/")
        return (len(components) == 3 and components[:2] == [case, self.provider]
                and re.fullmatch(r"[1-9][0-9]{0,9}", components[2]) is not None)

    def register(self, payload):
        if not isinstance(payload, dict) or payload.keys() - {"case_id", "operation_id", "awakeable_id", "invocation_id"}:
            return 422, {"error": "invalid_registration"}
        case, awakeable, invocation = (payload.get(k) for k in ("case_id", "awakeable_id", "invocation_id"))
        operation = payload.get("operation_id")
        if (not self.valid_operation(case, operation) or not isinstance(case, str) or not CASE_ID.fullmatch(case)
                or not isinstance(awakeable, str) or not AWAKEABLE_ID.fullmatch(awakeable)
                or (invocation is not None and
                    (not isinstance(invocation, str) or not INVOCATION_ID.fullmatch(invocation)))):
            return 422, {"error": "invalid_registration"}
        with self.connect() as db:
            db.execute("INSERT OR IGNORE INTO callbacks(operation_id,awakeable_id,invocation_id) VALUES(?,?,?)",
                       (operation, awakeable, invocation))
            row = db.execute("SELECT * FROM callbacks WHERE operation_id = ?", (operation,)).fetchone()
            if row is None or row["awakeable_id"] != awakeable or row["invocation_id"] != invocation:
                return 409, {"error": "registration_conflict"}
            return 202, status(row)[1]

    def read(self, operation):
        with self.connect() as db:
            return status(db.execute("SELECT * FROM callbacks WHERE operation_id = ?", (operation,)).fetchone())

    def choose(self, operation, response, reply):
        # Choose once and persist the exact vendor result (including relative dates)
        # in the same transaction as the delivery obligation.
        callback = json.dumps({"status": reply.status, "body": reply.payload}, separators=(",", ":"))
        with self.connect() as db:
            db.execute("""UPDATE callbacks SET response = ?, callback = ?
                          WHERE operation_id = ? AND response IS NULL""", (response, callback, operation))
            row = db.execute("SELECT * FROM callbacks WHERE operation_id = ?", (operation,)).fetchone()
            if row is None:
                return status(row)
            if row["response"] != response:
                return 409, {"error": "response_already_chosen"}
        self.changed.set()
        return (200 if row["sent"] else 202), status(row)[1]

    def deliver(self):
        while not self.stopped.is_set():
            self.changed.clear()
            if self.stopped.is_set():
                return
            try:
                with self.connect() as db:
                    row = db.execute("""SELECT * FROM callbacks WHERE callback IS NOT NULL AND sent = 0
                                        ORDER BY next_attempt LIMIT 1""").fetchone()
                if row is None:
                    self.changed.wait()
                    continue
                delay = row["next_attempt"] - time.time()
                if delay > 0:
                    self.changed.wait(delay)
                    continue
                delivered = self.send(row)
                with self.connect() as db:
                    if delivered:
                        db.execute("UPDATE callbacks SET sent = 1 WHERE operation_id = ?", (row["operation_id"],))
                    else:
                        backoff = min(30, 0.5 * 2 ** min(6, row["failures"]))
                        db.execute("""UPDATE callbacks SET failures = failures + 1, next_attempt = ?
                                      WHERE operation_id = ?""", (time.time() + backoff, row["operation_id"]))
            except sqlite3.OperationalError:
                # A temporary database failure must not retire the delivery worker.
                self.stopped.wait(1)

    def send(self, row):
        client = http.client.HTTPSConnection if self.ingress.scheme == "https" else http.client.HTTPConnection
        conn = client(self.ingress.hostname, self.ingress.port, timeout=3)
        try:
            path = self.ingress.path.rstrip("/") + "/restate/awakeables/" + row["awakeable_id"] + "/resolve"
            conn.request("POST", path, row["callback"].encode(), {"Content-Type": "application/json"})
            return 200 <= conn.getresponse().status < 300
        except (OSError, http.client.HTTPException):
            return False
        finally:
            conn.close()
