"""Local institution evidence issuer. Only synthetic capture endpoints live under /demo."""
import hmac
import json
import os
import sqlite3
import uuid
from fixtures import supports
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

METHODS = {"document": "document_verification", "face": "face_comparison",
           "address": "address_verification", "sanctions": "sanctions_screening"}


class Invalid(ValueError):
    pass


class Conflict(ValueError):
    pass


def identifier(value):
    if not isinstance(value, str) or not value.strip() or len(value.encode()) > 256 or not value.isprintable():
        raise Invalid()
    return value


def scenario(kind, value):
    if value != "interactive" and not supports(kind, value):
        raise Invalid()
    return "demo:" + value


class Registry:
    def __init__(self, path):
        self.path = path
        with closing(sqlite3.connect(path)) as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS assets(ref TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS provider_bindings(
                    ref TEXT PRIMARY KEY REFERENCES assets(ref), candidates TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS subjects(tenant TEXT, subject TEXT, document_ref TEXT,
                    screening_ref TEXT, PRIMARY KEY(tenant,subject));
                CREATE TABLE IF NOT EXISTS screening_methods(tenant TEXT,subject TEXT,method TEXT NOT NULL,
                    PRIMARY KEY(tenant,subject));
                CREATE TABLE IF NOT EXISTS strict_screening_documents(
                    document_ref TEXT PRIMARY KEY, replacement_scenario TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS receipts(tenant TEXT, request_key TEXT,
                    request TEXT, ref TEXT, PRIMARY KEY(tenant,request_key));
            """)

    @staticmethod
    def asset(db, ref, tenant, subject, kind):
        row = db.execute("SELECT record FROM assets WHERE ref=?", (identifier(ref),)).fetchone()
        if row:
            asset = json.loads(row[0])
            if (asset["tenant_id"], asset["subject_ref"], asset["kind"]) == (tenant, subject, kind):
                return asset
        raise Invalid()

    def issue(self, key, body):
        tenant, subject = identifier(body.get("institution")), identifier(body.get("subject_ref"))
        kind = body.get("kind")
        if kind not in ("document", "face", "address"):
            raise Invalid()
        request = json.dumps(body, sort_keys=True, separators=(",", ":"))
        with closing(sqlite3.connect(self.path)) as db, db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute("SELECT request,ref FROM receipts WHERE tenant=? AND request_key=?",
                             (tenant, identifier(key))).fetchone()
            if row:
                if row[0] != request:
                    raise Conflict()
                return {"evidence_ref": row[1]}
            ref = str(uuid.uuid4())
            record = dict(tenant_id=tenant, subject_ref=subject, kind=kind, evidence_ref=ref,
                          revision=ref, provider_ref=scenario(kind, body.get("scenario", "pass")),
                          provider=identifier(body.get("provider", kind)), method=METHODS[kind], document_revision=None, portrait_revision=None)
            if kind == "document":
                record.update(document_revision=ref, portrait_revision="portrait-" + ref)
                if body.get("strict_screening") is True:
                    replacement = scenario("sanctions", body.get("replacement_scenario", "pass"))
                    db.execute("INSERT INTO strict_screening_documents VALUES(?,?)", (ref, replacement))
                db.execute("INSERT OR REPLACE INTO subjects VALUES(?,?,?,?)",
                           (tenant, subject, ref, scenario("sanctions", body.get("screening_scenario", "pass"))))
            elif kind == "face":
                document_ref = body.get("document_ref")
                if "document_ref" not in body:
                    current = db.execute("SELECT document_ref FROM subjects WHERE tenant=? AND subject=?",
                                         (tenant, subject)).fetchone()
                    document_ref = current[0] if current else None
                doc = self.asset(db, document_ref, tenant, subject, "document")
                record.update(document_revision=doc["revision"], portrait_revision=doc["portrait_revision"])
            candidates = []
            if "providers" in body:
                providers = body["providers"]
                if (kind != "address" or not isinstance(providers, list) or not providers
                        or not all(p in ("address-a", "address-b") for p in providers)
                        or len(set(providers)) != len(providers)):
                    raise Invalid()
                options = body.get("provider_scenarios", {})
                if not isinstance(options, dict) or not set(options).issubset(providers):
                    raise Invalid()
                for provider in providers:
                    fixture = options.get(provider, body.get("scenario", "pass"))
                    if not supports(kind, fixture):
                        raise Invalid()
                    candidates.append(dict(record, provider=provider,
                                           method="address_verification_" + provider[-1],
                                           provider_ref=f"demo:{provider}:{fixture}"))
                record = candidates[0]
            db.execute("INSERT INTO assets VALUES(?,?)", (ref, json.dumps(record)))
            if candidates:
                db.execute("INSERT INTO provider_bindings VALUES(?,?)", (ref, json.dumps(candidates)))
            db.execute("INSERT INTO receipts VALUES(?,?,?,?)", (tenant, key, request, ref))
            return {"evidence_ref": ref}

    def set_screening(self, body):
        with closing(sqlite3.connect(self.path)) as db, db:
            result = db.execute("UPDATE subjects SET screening_ref=? WHERE tenant=? AND subject=?",
                                (scenario("sanctions", body.get("scenario")), identifier(body.get("institution")),
                                 identifier(body.get("subject_ref"))))
            if result.rowcount != 1:
                raise Invalid()
            db.execute("INSERT OR REPLACE INTO screening_methods VALUES(?,?,?)",
                       (body["institution"], body["subject_ref"], identifier(body.get("method", METHODS["sanctions"]))))
        return {"updated": True}

    @staticmethod
    def screening_candidates(db, tenant, subject, screening):
        approved = db.execute("""SELECT d.replacement_scenario FROM strict_screening_documents d
            JOIN subjects s ON s.document_ref=d.document_ref
            WHERE s.tenant=? AND s.subject=? AND s.document_ref=?""",
            (tenant, subject, screening["document_revision"])).fetchone()
        if not approved:
            return [screening]
        return [dict(screening, provider=provider, provider_ref=f"demo:{provider}:{locator.split(':', 1)[1]}")
                for provider, locator in [("sanctions-a", screening["provider_ref"]), ("sanctions-b", approved[0])]]

    def resolve(self, body, version=2):
        tenant, subject = identifier(body.get("institution")), identifier(body.get("subject_ref"))
        supplied = body.get("evidence")
        if not isinstance(supplied, dict) or not set(supplied) <= {"document", "face", "address"}:
            raise Invalid()
        with closing(sqlite3.connect(self.path)) as db, db:
            db.execute("BEGIN")
            evidence = {k: self.asset(db, ref, tenant, subject, k) for k, ref in supplied.items()}
            row = db.execute("SELECT document_ref,screening_ref FROM subjects WHERE tenant=? AND subject=?",
                             (tenant, subject)).fetchone()
            doc = self.asset(db, row[0], tenant, subject, "document") if row else None
            screening = None
            if doc:
                override = db.execute("SELECT method FROM screening_methods WHERE tenant=? AND subject=?",
                                      (tenant, subject)).fetchone()
                screening = dict(doc, kind="sanctions", evidence_ref="identity-" + doc["revision"],
                                 method=override[0] if override else METHODS["sanctions"], provider="sanctions", provider_ref=row[1])
            snapshot = dict(evidence=evidence, current_document_revision=doc and doc["revision"],
                        current_portrait_revision=doc and doc["portrait_revision"], screening=screening)

            if version == 3:
                candidates = {}
                for kind, item in evidence.items():
                    bindings = db.execute("SELECT candidates FROM provider_bindings WHERE ref=?",
                                          (item["evidence_ref"],)).fetchone()
                    candidates[kind] = json.loads(bindings[0]) if bindings else [item]
                if screening:
                    candidates["sanctions"] = self.screening_candidates(db, tenant, subject, screening)
                snapshot.update(version=3, provider_candidates=candidates)
            return snapshot


class Handler(BaseHTTPRequestHandler):
    def reply(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        self.reply(200 if self.path == "/health" else 404, {})

    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 16384:
                raise Invalid()
            body = json.loads(self.rfile.read(length))
            if not isinstance(body, dict):
                raise Invalid()
            if self.path == "/demo/evidence":
                self.reply(201, self.server.registry.issue(self.headers.get("Idempotency-Key"), body))
            elif self.path == "/demo/screening":
                self.reply(200, self.server.registry.set_screening(body))
            elif self.path in ("/v2/resolve", "/v3/resolve"):
                token = self.headers.get("Authorization", "").encode()
                if not hmac.compare_digest(token, self.server.auth):
                    return self.reply(401, {})
                v3 = (self.path == "/v3/resolve" or self.headers.get("Accept") ==
                      "application/vnd.ekyc.issuer.v3+json")
                self.reply(200, self.server.registry.resolve(body, 3 if v3 else 2))
            else:
                self.reply(404, {})
        except Conflict:
            self.reply(409, {"error": "key_reused"})
        except (ValueError, TypeError, KeyError):
            self.reply(422, {"error": "invalid_evidence"})
        except sqlite3.Error:
            self.reply(503, {"error": "issuer_unavailable"})

    def log_message(self, *_):
        pass


if __name__ == "__main__":
    server = ThreadingHTTPServer((os.getenv("ISSUER_BIND", "127.0.0.1"), int(os.getenv("PORT", "9805"))), Handler)
    server.registry = Registry(os.getenv("ISSUER_DB", "/tmp/ekyc-issuer.sqlite3"))
    server.auth = ("Bearer " + os.environ["EKYC_EVIDENCE_TOKEN"]).encode()
    server.serve_forever()
