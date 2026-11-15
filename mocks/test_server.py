"""Exercise mock contracts using actual HTTP connections on four ephemeral ports."""

import http.client
import json
import socket
import threading
import unittest
from datetime import date, datetime, timedelta, timezone
from urllib.parse import quote

from server import ProviderServer, ROUTES


class MockContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.servers = {}
        for provider in ROUTES:
            server = ProviderServer(("127.0.0.1", 0), provider, timeout_delay=0.35)
            thread = threading.Thread(target=server.serve_forever, args=(0.02,), daemon=True)
            thread.start()
            cls.servers[provider] = server
            cls.addClassCleanup(thread.join, 1)
            cls.addClassCleanup(server.server_close)
            cls.addClassCleanup(server.shutdown)

    def send(self, provider, scenario="pass", key=None, timeout=2, path=None, payload=None):
        endpoint, field = ROUTES[provider]
        body = json.dumps({field: "demo:" + scenario} if payload is None else payload)
        key = self.id() if key is None else key
        connection = http.client.HTTPConnection(
            "127.0.0.1", self.servers[provider].server_port, timeout=timeout,
        )
        try:
            connection.request("POST", path or endpoint, body, {
                "Content-Type": "application/json",
                "Idempotency-Key": key,
            })
            response = connection.getresponse()
            return response.status, json.load(response), dict(response.getheaders())
        finally:
            connection.close()

    def test_contracts(self):
        today = datetime.now(timezone.utc).date()
        status, document, _ = self.send("document")
        self.assertEqual(status, 200)
        self.assertEqual(document["status"], "PASS")
        self.assertGreater(date.fromisoformat(document["expires_on"]), today)
        self.assertGreater(document["confidence"], 85)
        self.assertEqual(self.send("face")[:2], (200, {
            "match_status": "PASS", "confidence": 95, "similarity_score": 96.5,
        }))
        status, address, _ = self.send("address")
        self.assertEqual((status, address["status"]), (200, "PASS"))
        self.assertGreater(address["confidence"], 80)
        self.assertEqual(date.fromisoformat(address["proof_date"]), today - timedelta(days=10))
        self.assertEqual(self.send("sanctions")[:2], (200, {
            "screening_status": "CLEAR", "match_count": 0,
        }))

    def test_contract_rejects_wrong_route_and_schema(self):
        self.assertEqual(self.send("face", path="/documents/verify")[0], 404)
        self.assertEqual(self.send("face", payload={"document_ref": "demo:pass"})[0], 422)
        self.assertEqual(self.send("face", key="")[0], 400)
        self.assertEqual(self.send("face", scenario="not-a-fixture")[0], 422)

    def test_subject_scoped_operation_key(self):
        original = self.send("sanctions", "hit")
        self.assertEqual(original[:2], (200, {"screening_status": "HIT", "match_count": 1}))
        self.assertEqual(original[:2], self.send("sanctions", "hit")[:2])
        self.assertEqual(self.send("sanctions", "pass")[0], 409)

    def test_review_and_expiry_fields(self):
        today = datetime.now(timezone.utc).date()
        _, expired, _ = self.send("document", "expired")
        self.assertLess(date.fromisoformat(expired["expires_on"]), today)
        _, stale, _ = self.send("address", "stale")
        self.assertLess(date.fromisoformat(stale["proof_date"]), today - timedelta(days=90))
        self.assertEqual(self.send("face", "threshold")[1]["similarity_score"], 85.0)
        self.assertEqual(self.send("sanctions", "inconclusive")[0], 422)
        self.assertEqual(self.send("face", "review", key=self.id() + "-review")[1]["match_status"], "REVIEW")

    def test_throttling_and_service_outage(self):
        status, _, headers = self.send("face", "throttle-once")
        self.assertEqual((status, headers["Retry-After"]), (429, "1"))
        self.assertEqual(self.send("face", "throttle-once")[0], 200)
        self.assertEqual(self.send("address", "unavailable-once")[0], 503)
        self.assertEqual(self.send("address", "unavailable-once")[0], 200)
        self.assertEqual(self.send("sanctions", "unavailable")[0], 503)
        self.assertEqual(self.send("sanctions", "unavailable")[0], 503)

    def test_client_timeout_keeps_idempotent_result(self):
        with self.assertRaises((TimeoutError, socket.timeout)):
            self.send("face", "timeout-once", timeout=0.07)
        recovered = self.send("face", "timeout-once")
        self.assertEqual((recovered[0], recovered[1]["match_status"]), (200, "PASS"))
        self.assertEqual(recovered[:2], self.send("face", "timeout-once")[:2])
        connection = http.client.HTTPConnection("127.0.0.1", self.servers["face"].server_port)
        try:
            connection.request("GET", "/__mock__/operations/" + quote(self.id(), safe=""))
            self.assertEqual(json.load(connection.getresponse()), {
                "attempts": 1, "cache_hits": 2, "result_saved": True,
            })
        finally:
            connection.close()


if __name__ == "__main__":
    unittest.main()
