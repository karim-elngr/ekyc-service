"""Check real HTTP responses and delivered events against the same OpenAPI files clients download."""

import copy
import json
import os
import uuid
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker, ValidationError
from journey_support import (
    API,
    CASES,
    ISSUER,
    LENDING,
    REVIEWER,
    checks,
    delivery,
    expect,
    http,
    issue,
    new_key,
    post,
    read,
)

CONTRACTS = Path(__file__).resolve().parents[1] / "apps/portal/public"
API_SPEC = json.loads((CONTRACTS / "openapi.json").read_text())
ISSUER_SPEC = json.loads((CONTRACTS / "issuer-contract.json").read_text())


def dereference(document, spec):
    while "$ref" in spec:
        target = document
        for token in spec["$ref"][2:].split("/"):
            target = target[token]
        spec = target
    return spec


def validate(document, schema, value):
    # Local component references share the root of the distributed contract.
    Draft202012Validator(
        {"components": document["components"], "allOf": [schema]}, format_checker=FormatChecker()
    ).validate(value)


def checked(document, path, method, response, expected):
    status, headers, body = response
    if status != expected:
        raise AssertionError(f"{method} {path} expected {expected}, got {status}: {body}")
    definition = dereference(document, document["paths"][path][method]["responses"][str(status)])
    content = headers["Content-Type"].split(";")[0]
    validate(document, definition["content"][content]["schema"], body)
    for name, header in definition.get("headers", {}).items():
        if header.get("required") or headers.get(name) is not None:
            validate(document, header["schema"], headers.get(name))
    return body


def validate_event(event):
    operation = API_SPEC["webhooks"][event["event_type"]]["post"]
    schema = operation["requestBody"]["content"]["application/json"]["schema"]
    validate(API_SPEC, schema, event)
    # The receiver also checks the actual bearer and Idempotency-Key for every delivery.
    return schema


def main():
    portal = os.getenv("EKYC_PORTAL_URL")
    if portal:
        for name, spec in [("openapi.json", API_SPEC), ("issuer-contract.json", ISSUER_SPEC)]:
            published = expect(http("GET", portal.rstrip("/") + "/" + name), 200)[1]
            assert published == spec, f"Hosted {name} differs from the checked contract"
    subject = "contract-" + uuid.uuid4().hex
    key = new_key()
    opening = {"subject_ref": subject, "use_case": "loan_application"}
    opened = http("POST", CASES, opening, LENDING, key)
    receipt = checked(API_SPEC, "/v2/cases", "post", opened, 202)
    case_id = receipt["case_id"]
    assert opened[1]["Location"] == receipt["location"] == "/v2/cases/" + case_id
    url = CASES + "/" + case_id
    initial = read(case_id, LENDING, lambda case: case["state"] == "AWAITING_EVIDENCE")
    validate_event(delivery(initial, "action"))
    checked(API_SPEC, "/v2/cases/{id}", "get", http("GET", url, token=LENDING), 200)
    for path, actual in [
        ("/v2/cases", CASES + "?page_size=101"),
        ("/v2/cases/overview", CASES + "/overview?from=invalid"),
        ("/v2/cases/{id}/history", url + "/history?limit=0"),
    ]:
        checked(API_SPEC, path, "get", http("GET", actual, token=LENDING), 400)

    issuer_token = os.getenv("EKYC_EVIDENCE_TOKEN", "local-demo-evidence-issuer")
    refs = {}
    for kind in (None, "document", "face", "address"):
        if kind:
            opts = (
                {"screening_scenario": "hit"}
                if kind == "document"
                else ({"document_ref": refs["document"]} if kind == "face" else {})
            )
            refs[kind] = issue(subject, kind, **opts)
        request = {"institution": "demo-bank", "subject_ref": subject, "evidence": dict(refs)}
        validate(ISSUER_SPEC, {"$ref": "#/components/schemas/ResolveRequest"}, request)
        checked(
            ISSUER_SPEC,
            "/v2/resolve",
            "post",
            http("POST", ISSUER + "/v2/resolve", request, issuer_token),
            200,
        )
    expect(
        http(
            "POST",
            ISSUER + "/v2/resolve",
            {**request, "subject_ref": subject + "-other"},
            issuer_token,
        ),
        422,
    )
    post(url + "/evidence", LENDING, {"evidence": refs})
    read(
        case_id,
        LENDING,
        lambda c: c["state"] == "AWAITING_REVIEW" and checks(c)["address"]["status"] == "PASSED",
    )
    handoff = checked(
        API_SPEC, "/v2/cases/{id}/review", "get", http("GET", url + "/review", token=REVIEWER), 200
    )
    op = checks(handoff)["sanctions"]["operation_id"]
    staff = "local-demo-reviewer-alice"
    checked(
        API_SPEC,
        "/v2/staff/session",
        "get",
        http("GET", API + "/v2/staff/session", token=staff),
        200,
    )
    queue_url = API + "/v2/investigations"
    checked(
        API_SPEC, "/v2/investigations", "get", http("GET", queue_url + "?limit=0", token=staff), 400
    )
    queue = checked(
        API_SPEC,
        "/v2/investigations",
        "get",
        http("GET", queue_url + "?limit=100", token=staff),
        200,
    )
    item = next(row for row in queue["items"] if row["operation_id"] == op)
    receipt = checked(
        API_SPEC,
        "/v2/investigations/acknowledgement",
        "put",
        http("PUT", queue_url + "/acknowledgement", {"work_id": item["work_id"]}, staff),
        200,
    )
    assert receipt["acknowledged_by"] == "reviewer-alice"
    payload = {
        "check": "sanctions",
        "operation_id": op,
        "disposition": "clear",
        "reason": "Checked the identity.\n" + "ي" * 300,
        "review_ref": "ticket-" + subject,
    }
    stale = checked(
        API_SPEC,
        "/v2/cases/{id}/review",
        "post",
        http(
            "POST", url + "/review", {**payload, "operation_id": op + "-old"}, REVIEWER, new_key()
        ),
        409,
    )
    assert stale["error"] == "stale_operation", stale
    conflict = checked(
        API_SPEC,
        "/v2/cases",
        "post",
        http("POST", CASES, {**opening, "subject_ref": "other-" + subject}, LENDING, key),
        409,
    )
    assert conflict["error"] == "idempotency_key_reused", conflict
    post(url + "/review", REVIEWER, payload)
    final = read(case_id, LENDING, lambda c: c["state"] == "APPROVED")
    validate_event(delivery(final, "decided"))
    closed = checked(
        API_SPEC,
        "/v2/cases/{id}/review",
        "post",
        http("POST", url + "/review", payload, REVIEWER, new_key()),
        409,
    )
    assert closed["error"] == "case_already_decided", closed

    # A superseded match enters the independent reviewer queue and never changes a final decision.
    from late_screen_smoke import reviewed_identity_replaced

    alert = reviewed_identity_replaced()
    schema = validate_event(alert)
    broken = copy.deepcopy(alert)
    del broken["operation_id"]
    try:
        validate(API_SPEC, schema, broken)
    except ValidationError:
        pass
    else:
        raise AssertionError("A changed delivered event passed contract validation")
    print(
        "OpenAPI verified against live issuer, HTTP errors, command receipts, reviewer reads and all three webhook types."
    )


if __name__ == "__main__":
    main()
