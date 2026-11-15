"""Exercise the presenter-to-mock and mock-to-Restate HTTP hops."""

import http.client
import json
import os
import tempfile
import threading
import time
import unittest
from urllib.parse import urlencode
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from server import ProviderServer


CASE = "v2~demo~" + "a" * 64
AWAKEABLE = "sign_1aBCdEfGhIj0123456789"
INVOCATION = "inv_1aBCdEfGhIj0123456789"
BASE = "/__mock__/interactive/"
OPERATION = CASE + "/face/1"


def endpoint(operation=OPERATION):
    return BASE + CASE + "?" + urlencode({"operation_id": operation})


class CallbackReceiver(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with self.server.condition:
            self.server.attempts.append((self.path, body))
            self.server.condition.notify_all()
            if self.server.lose_first_ack and len(self.server.attempts) == 1:
                self.close_connection = True
                return
        self.send_response(200 if self.server.available else 503)
        self.end_headers()

    def log_message(self, *_):
        pass


@contextmanager
def serving(server):
    thread = threading.Thread(target=server.serve_forever, args=(0.02,), daemon=True)
    thread.start()
    try:
        yield server.server_port
    finally:
        server.shutdown()
        server.server_close()
        thread.join(1)


def request(port, method, path, payload=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        body = None if payload is None else json.dumps(payload)
        conn.request(method, path, body, {"Content-Type": "application/json"})
        reply = conn.getresponse()
        return reply.status, json.load(reply)
    finally:
        conn.close()


class InteractiveChecksTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.temp = temp.name
        self.receiver = ThreadingHTTPServer(("127.0.0.1", 0), CallbackReceiver)
        self.receiver.available = True
        self.receiver.lose_first_ack = False
        self.receiver.condition = threading.Condition()
        self.receiver.attempts = []
        context = serving(self.receiver)
        self.ingress = "http://127.0.0.1:" + str(context.__enter__())
        self.addCleanup(context.__exit__, None, None, None)

    @contextmanager
    def provider(self):
        with patch.dict(os.environ, {
            "RESTATE_INGRESS_URL": self.ingress,
            "INTERACTIVE_DB_PATH": os.path.join(self.temp, "face.sqlite3"),
        }):
            instance = ProviderServer(("127.0.0.1", 0), "face", 1)
        with serving(instance) as port:
            yield port

    def register(self, port, **overrides):
        payload = {"operation_id": OPERATION, "case_id": CASE, "awakeable_id": AWAKEABLE, "invocation_id": INVOCATION}
        return request(port, "POST", BASE + "register", dict(payload, **overrides))

    def wait_for_sent(self, port):
        end = time.monotonic() + 5
        while time.monotonic() < end:
            status, data = request(port, "GET", endpoint())
            if status == 200 and data["status"] == "sent":
                return data
            time.sleep(0.02)
        self.fail("mock did not deliver its callback")

    def test_wait_select_once_and_deliver_vendor_response(self):
        with self.provider() as port:
            self.assertEqual(request(port, "GET", endpoint())[0], 404)
            self.assertEqual(self.register(port)[0], 202)
            self.assertEqual(self.register(port)[0], 202)
            self.assertEqual(self.register(port, awakeable_id=AWAKEABLE + "x")[0], 409)
            self.assertEqual(self.register(port, awakeable_id="../../unsafe")[0], 422)
            self.assertEqual(request(port, "GET", endpoint()), (200, {
                "status": "awaiting_response", "response": None, "invocation_id": INVOCATION,
            }))
            for response in ("demo:hit", "demo:timeout-once", "demo:missing", None):
                self.assertEqual(request(port, "POST", endpoint(), {"response": response})[0], 422)

            def submit(response):
                return request(port, "POST", endpoint(), {"response": response})

            with ThreadPoolExecutor(max_workers=2) as executor:
                results = list(executor.map(submit, ["demo:threshold", "demo:review"]))
            self.assertEqual(sorted(code for code, _ in results), [202, 409])
            choice = self.wait_for_sent(port)["response"]
            self.assertEqual(submit(choice)[0], 200)
            self.assertEqual(len(self.receiver.attempts), 1)
            path, payload = self.receiver.attempts[0]
            self.assertEqual(path, "/restate/awakeables/" + AWAKEABLE + "/resolve")
            fields = json.loads(payload)
            self.assertEqual(fields["status"], 200)
            if choice == "demo:threshold":
                self.assertEqual(fields["body"]["similarity_score"], 85.0)
            else:
                self.assertEqual(fields["body"]["match_status"], "REVIEW")

    def test_attempts_bind_distinct_callbacks_to_their_case_and_provider(self):
        with self.provider() as port:
            second = CASE + "/face/2"
            self.assertEqual(self.register(port)[0], 202)
            self.assertEqual(self.register(port, operation_id=second, awakeable_id=AWAKEABLE + "2")[0], 202)
            self.assertEqual(self.register(port, operation_id=CASE + "/document/1")[0], 422)
            self.assertEqual(self.register(port, operation_id=OPERATION.replace("demo", "other"))[0], 422)
            self.assertEqual(request(port, "POST", endpoint(second), {"response": "demo:review"})[0], 202)
            self.assertEqual(request(port, "GET", endpoint())[1]["response"], None)
            self.assertEqual(request(port, "GET", endpoint(second))[1]["response"], "demo:review")
            self.assertNotEqual(request(port, "GET", BASE + CASE)[0], 200)

    def test_pending_and_undelivered_callbacks_survive_restart(self):
        with self.provider() as port:
            self.assertEqual(self.register(port)[0], 202)
        self.receiver.available = False
        with self.provider() as port:
            self.assertEqual(request(port, "GET", endpoint())[1]["status"], "awaiting_response")
            self.assertEqual(request(port, "POST", endpoint(), {"response": "demo:unavailable"})[0], 202)
            with self.receiver.condition:
                self.assertTrue(self.receiver.condition.wait_for(lambda: self.receiver.attempts, timeout=2))
            self.assertEqual(request(port, "GET", endpoint())[1]["status"], "sending")
        self.receiver.available = True
        with self.provider() as port:
            self.assertEqual(self.wait_for_sent(port)["response"], "demo:unavailable")
        self.assertGreaterEqual(len(self.receiver.attempts), 2)
        self.assertEqual(len(set(self.receiver.attempts)), 1)
        self.assertEqual(json.loads(self.receiver.attempts[-1][1])["status"], 503)

    def test_ambiguous_delivery_retries_the_original_callback(self):
        self.receiver.lose_first_ack = True
        with self.provider() as port:
            self.assertEqual(self.register(port)[0], 202)
            self.assertEqual(request(port, "POST", endpoint(), {"response": "demo:pass"})[0], 202)
            self.assertEqual(self.wait_for_sent(port)["response"], "demo:pass")
        self.assertEqual(len(self.receiver.attempts), 2)
        self.assertEqual(self.receiver.attempts[0], self.receiver.attempts[1])
