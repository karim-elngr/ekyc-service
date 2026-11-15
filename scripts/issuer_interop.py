"""A standalone synthetic institution resolver used by the interoperability drill.

Implements the published issuer OpenAPI contract using only Python's standard library.
The single identity is provisioned at startup. Its provider references select successful
responses from the local synthetic vendors. No production data or issuance endpoint.
"""
import hmac
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


METHODS = {
    "document": "document_verification",
    "face": "face_comparison",
    "address": "address_verification",
    "sanctions": "sanctions_screening",
}


class Lookup:
    def __init__(self, institution, subject):
        self.institution = institution
        self.subject = subject
        self.assets = {
            kind: {
                "tenant_id": institution, "subject_ref": subject, "kind": kind,
                "evidence_ref": "issued-" + kind, "revision": kind + "-r1",
                "provider": kind, "provider_ref": "demo:pass", "method": method,
                "document_revision": None if kind == "address" else "document-r1",
                "portrait_revision": "portrait-r1" if kind in ("document", "face") else None,
            }
            for kind, method in METHODS.items()
        }

    def resolve(self, request):
        if not isinstance(request, dict) or set(request) != {"institution", "subject_ref", "evidence"}:
            return 422, {"error": "invalid_evidence"}
        institution, subject, requested = (
            request["institution"], request["subject_ref"], request["evidence"]
        )
        if not all(isinstance(value, str) and value.strip() for value in (institution, subject)):
            return 422, {"error": "invalid_evidence"}
        if subject == self.subject + "-outage":
            return 503, {"error": "issuer_unavailable"}
        owned = (institution, subject) == (self.institution, self.subject)
        if not isinstance(requested, dict) or any(
            not owned or kind not in ("document", "face", "address")
            or ref != self.assets[kind]["evidence_ref"] for kind, ref in requested.items()
        ):
            return 422, {"error": "invalid_evidence"}
        return 200, {
            "evidence": {kind: self.assets[kind] for kind in requested},
            "current_document_revision": "document-r1" if owned else None,
            "current_portrait_revision": "portrait-r1" if owned else None,
            "screening": self.assets["sanctions"] if owned else None,
        }


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v2/resolve":
            return self.reply(404, {})
        supplied = self.headers.get("Authorization", "").encode()
        if not hmac.compare_digest(supplied, self.server.credential):
            return self.reply(401, {})
        try:
            length = int(self.headers.get("Content-Length", 0))
            if not 0 < length <= 16384:
                raise ValueError()
            request = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeError):
            return self.reply(422, {"error": "invalid_evidence"})
        self.reply(*self.server.lookup.resolve(request))

    def reply(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_):
        pass


if __name__ == "__main__":
    with ThreadingHTTPServer((os.environ.get("ISSUER_BIND", "127.0.0.1"),
                              int(os.environ.get("PORT", "9805"))), Handler) as server:
        server.lookup = Lookup("demo-bank", os.environ["ISSUER_SUBJECT"])
        server.credential = ("Bearer " + os.environ["EKYC_EVIDENCE_TOKEN"]).encode()
        server.serve_forever()
