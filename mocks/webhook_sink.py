#!/usr/bin/env python3
"""Single-consumer development webhook inbox. Its SQLite file survives restarts."""
import hmac
import json
import os
import sqlite3
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit


class Inbox(BaseHTTPRequestHandler):
    def respond(self, code, obj):
        body = json.dumps(obj, separators=(",", ":")).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def authorized(self):
        have = self.headers.get("Authorization", "").encode()
        if hmac.compare_digest(have, self.server.authorization):
            return True
        self.respond(401, {"error": "unauthorized"})
        return False

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/health":
            self.respond(200, {"ok": True})
            return
        if not self.authorized():
            return
        if not path.startswith("/events/"):
            self.respond(404, {"error": "not found"})
            return
        event_id = unquote(path[len("/events/"):])
        with sqlite3.connect(self.server.db) as db:
            row = db.execute("SELECT payload FROM events WHERE id = ?", (event_id,)).fetchone()
        if row:
            self.respond(200, json.loads(row[0]))
        else:
            self.respond(404, {"error": "not found"})

    def do_POST(self):
        if not self.authorized():
            return
        if urlsplit(self.path).path != "/events":
            self.respond(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", ""))
            if length <= 0 or length > 65536:
                raise ValueError
            event = json.loads(self.rfile.read(length))
            event_id = self.headers.get("Idempotency-Key")
            if not isinstance(event, dict) or not event_id or event.get("event_id") != event_id:
                raise ValueError
            payload = json.dumps(event, sort_keys=True, separators=(",", ":"), allow_nan=False)
        except (ValueError, UnicodeError):
            self.respond(400, {"error": "invalid event or idempotency key"})
            return
        # Acknowledge only after committing. Recovery can safely resend the same event.
        with sqlite3.connect(self.server.db) as db:
            cursor = db.execute("INSERT INTO events(id, payload) VALUES(?, ?) ON CONFLICT DO NOTHING",
                                (event_id, payload))
            created = cursor.rowcount == 1
            existing = db.execute("SELECT payload FROM events WHERE id = ?", (event_id,)).fetchone()[0]
        if existing != payload:
            self.respond(409, {"error": "key reused for a different event"})
            return
        self.respond(201 if created else 200, {"event_id": event_id})

    def log_message(self, fmt, *args):
        # The demo avoids logging event bodies, request paths and authorization.
        pass


def main():
    token = os.environ.get("WEBHOOK_TOKEN", "")
    if not token.strip():
        raise SystemExit("WEBHOOK_TOKEN is required")
    db_path = os.environ.get("WEBHOOK_DB", "/tmp/events.sqlite3")
    with sqlite3.connect(db_path) as db:
        db.execute("CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, payload TEXT NOT NULL)")
    server = ThreadingHTTPServer(("0.0.0.0", int(os.environ.get("PORT", "8110"))), Inbox)
    server.authorization = f"Bearer {token}".encode()
    server.db = db_path
    server.serve_forever()


if __name__ == "__main__":
    main()
