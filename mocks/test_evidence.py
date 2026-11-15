import tempfile
import unittest
from evidence import Registry, Conflict, Invalid


class EvidenceRegistryTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.reg = Registry(tmp.name + "/issuer.db")
        self.owner = {"institution": "bank", "subject_ref": "subject"}

    def issue(self, key, kind, **kwargs):
        return self.reg.issue(key, dict(self.owner, kind=kind, **kwargs))["evidence_ref"]

    def test_binding_replay_and_version_replacement(self):
        doc = self.issue("one", "document")
        self.assertEqual(self.issue("one", "document"), doc)
        with self.assertRaises(Conflict):
            self.issue("one", "address")
        face = self.issue("capture", "face", document_ref=doc)
        old = self.reg.resolve(dict(self.owner, evidence={"document": doc, "face": face}))
        self.assertEqual(old["evidence"]["face"]["document_revision"], old["current_document_revision"])
        self.assertEqual(old["screening"]["document_revision"], old["current_document_revision"])
        new_doc = self.issue("two", "document")
        newer = self.reg.resolve(dict(self.owner, evidence={"document": new_doc}))
        self.assertNotEqual(old["current_document_revision"], newer["current_document_revision"])
        self.assertNotEqual(old["current_portrait_revision"], newer["current_portrait_revision"])

    def test_screening_method_drift_is_subject_scoped_and_can_be_restored(self):
        self.issue("doc", "document")
        self.reg.set_screening(dict(self.owner, scenario="interactive", method="renamed_screen"))
        snapshot = self.reg.resolve(dict(self.owner, evidence={}))
        self.assertEqual(snapshot["screening"]["method"], "renamed_screen")
        self.reg.set_screening(dict(self.owner, scenario="pass"))
        self.assertEqual(self.reg.resolve(dict(self.owner, evidence={}))["screening"]["method"],
                         "sanctions_screening")

    def test_references_are_scoped_even_with_the_same_customer_id(self):
        doc = self.issue("one", "document")
        for owner in [dict(self.owner, institution="other"), dict(self.owner, subject_ref="other")]:
            with self.assertRaises(Invalid):
                self.reg.resolve(dict(owner, evidence={"document": doc}))
            self.assertIsNone(self.reg.resolve(dict(owner, evidence={}))["screening"])
        with self.assertRaises(Invalid):
            self.reg.resolve(dict(self.owner, evidence={"address": doc}))

    def test_resume_face_capture_uses_current_subject_document(self):
        with self.assertRaises(Invalid):
            self.issue("no-document", "face")
        original = self.issue("original", "document")
        replacement = self.issue("replacement", "document")
        face = self.issue("resume", "face")
        result = self.reg.resolve(dict(self.owner, evidence={"face": face}))
        self.assertEqual(result["evidence"]["face"]["document_revision"], replacement)
        self.assertNotEqual(result["evidence"]["face"]["document_revision"], original)

    def test_rejects_unsupported_provider_scenarios_at_issuance(self):
        with self.assertRaises(Invalid):
            self.issue("bad", "address", scenario="expired")
        with self.assertRaises(Invalid):
            self.issue("bad-screen", "document", screening_scenario="stale")
        doc = self.issue("interactive", "document", scenario="interactive", screening_scenario="interactive")
        result = self.reg.resolve(dict(self.owner, evidence={"document": doc}))
        self.assertEqual(result["evidence"]["document"]["provider_ref"], "demo:interactive")
        self.assertEqual(result["screening"]["provider_ref"], "demo:interactive")
