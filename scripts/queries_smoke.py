"""Verify bounded, scoped browsing using the public HTTP API against real PostgreSQL."""
import os
import uuid
from datetime import datetime, timedelta, timezone
from urllib.parse import urlencode

from journey_support import API, CASES, OTHER, WALLET, expect, http, issue, new_key, open_loan, post, read

OBSERVER = os.environ.get("EKYC_OBSERVER_TOKEN", "local-demo-institution-observer")


def query(path, params, token=OBSERVER):
    return expect(http("GET", path + "?" + urlencode(params), token=token), 200)[1]


def main():
    subject = "query-customer-" + uuid.uuid4().hex
    start = datetime.now(timezone.utc) - timedelta(minutes=2)
    wallet = post(CASES, WALLET, {"subject_ref": subject, "use_case": "address_refresh",
                                 "evidence": {"address": issue(subject, "address")}})
    read(wallet, WALLET, lambda v: v["state"] == "APPROVED")
    loan = open_loan(subject)
    other = post(CASES, OTHER, {"subject_ref": subject, "use_case": "loan_application"})
    read(other, OTHER, lambda v: v["state"] == "AWAITING_EVIDENCE")
    params = {"from": start.isoformat(), "to": datetime.now(timezone.utc).isoformat(), "query": subject}
    first = query(CASES, dict(params, page_size=1, page=1, sort="created_at_asc"))
    second = query(CASES, dict(params, page_size=1, page=2, sort="created_at_asc"))
    assert [v["case_id"] for page in (first, second) for v in page["cases"]] == [wallet, loan], (first, second)
    assert first["total"] == second["total"] == 2
    descending = query(CASES, dict(params, sort="created_at_desc"))
    assert [v["case_id"] for v in descending["cases"]] == [loan, wallet]
    overview = query(CASES + "/overview", params)
    assert overview["total"] == 2 and overview["checks"], overview
    assert query(CASES + "/overview", dict(params, product="wallet", state="APPROVED"))["total"] == 1
    assert query(CASES, dict(params, use_case="not_a_real_use_case"))["total"] == 0
    assert query(CASES, params, WALLET)["total"] == query(CASES, params, OTHER)["total"] == 1
    earlier = {"from": (start - timedelta(minutes=1)).isoformat(), "to": start.isoformat()}
    assert query(CASES, {**params, **earlier})["total"] == 0
    assert not query(CASES + "/" + wallet + "/history", earlier)["revisions"]
    timeline = query(CASES + "/" + wallet + "/history", {"limit": 1})
    assert len(timeline["revisions"]) == 1 and timeline["next_before_version"] is not None, timeline
    previous = query(CASES + "/" + wallet + "/history", {"limit": 1, "before_version": timeline["next_before_version"]})
    assert previous["revisions"][0]["version"] < timeline["revisions"][0]["version"]
    for invalid in ({"page": 0}, {"page_size": 101}, {"sort": "subject"},
                    {"from": (start - timedelta(days=370)).isoformat(), "to": params["to"]}):
        expect(http("GET", CASES + "?" + urlencode(invalid), token=OBSERVER), 400)
    expect(http("GET", CASES + "/" + other, token=OBSERVER), 404)
    expect(http("GET", CASES + "/" + wallet + "/review", token=OBSERVER), 403)
    expect(http("POST", CASES, {"subject_ref": subject, "use_case": "onboarding"},
                token=OBSERVER, key=new_key()), 403)
    expect(http("GET", API + "/v1/cases", token=WALLET), 404)
    print("Combined overview and sorted pages agree; filters, limits, history, and institution boundaries verified.")


if __name__ == "__main__":
    main()
