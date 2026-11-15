"""Synthetic vendor contracts used by the local issuer and providers."""
from dataclasses import dataclass
from datetime import timedelta

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
    "face": {"threshold", "held-pass"},
    "address": {"stale", "threshold", "held-pass", "lost-held-pass"},
    "sanctions": {"hit", "inconclusive", "held-hit", "held-pass"},
}


@dataclass(frozen=True)
class Reply:
    status: int
    payload: dict
    retry_after: str | None = None


def outcome(provider, scenario, evaluated):
    today = evaluated.date()
    timestamp = evaluated.isoformat()
    if provider == "document":
        days_valid = -1 if scenario == "expired" else 365
        return Reply(200, {
            "evaluated_at": timestamp,
            "status": "REVIEW" if scenario == "review" else "PASS",
            "confidence": 85 if scenario == "threshold" else 98,
            "expires_on": (today + timedelta(days=days_valid)).isoformat(),
        })
    if provider == "face":
        return Reply(200, {
            "evaluated_at": timestamp,
            "match_status": "REVIEW" if scenario == "review" else "PASS",
            "confidence": 95,
            "similarity_score": 85.0 if scenario == "threshold" else 96.5,
        })
    if provider == "address":
        age = 91 if scenario == "stale" else 10
        return Reply(200, {
            "evaluated_at": timestamp,
            "status": "REVIEW" if scenario == "review" else "PASS",
            "confidence": 80 if scenario == "threshold" else 96,
            "proof_date": (today - timedelta(days=age)).isoformat(),
        })
    if scenario in {"inconclusive", "review"}:
        return Reply(422, {"error": "screening_inconclusive"})
    hit = scenario in {"hit", "held-hit"}
    return Reply(200, {
        "evaluated_at": timestamp,
        "screening_status": "HIT" if hit else "CLEAR",
        "match_count": int(hit),
    })


def supports(provider, scenario):
    return isinstance(scenario, str) and scenario in SHARED_FIXTURES | SPECIFIC_FIXTURES[provider]
