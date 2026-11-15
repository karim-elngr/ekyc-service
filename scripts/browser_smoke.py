"""Use the running portal to create, acknowledge and decide cases as two individual reviewers."""

import os
import re
from urllib.parse import urlencode, urlsplit

from playwright.sync_api import expect as visible, sync_playwright

from journey_support import CASES, LENDING, WALLET, checks, issue, new_key, read, wait_result

PORTAL = os.getenv("EKYC_PORTAL_URL", "http://127.0.0.1:5173").rstrip("/")


def sign_in(page, token, portal=PORTAL):
    page.goto(portal + "/#staff")
    page.get_by_label("Personal access token").fill(token)
    page.get_by_role("button", name="Sign in").press("Enter")


def start_case(browser):
    lab = browser.new_page()
    try:
        lab.goto(PORTAL + "/#lab")
        lab.get_by_label("Scenario", exact=True).select_option("review")
        with lab.expect_response(
            lambda r: r.request.method == "POST"
            and urlsplit(r.url).path == "/product-demo/lending/cases"
        ) as receipt:
            lab.get_by_role("button", name="Start case").press("Enter")
        assert receipt.value.status == 202, receipt.value.text()
        case = receipt.value.json()["case_id"]
        view = read(
            case,
            LENDING,
            lambda v: v["state"] == "AWAITING_REVIEW"
            and all(checks(v)[k]["status"] == "PASSED" for k in ("document", "face", "address")),
        )
        return case, checks(view)["sanctions"]["operation_id"]
    finally:
        lab.close()


def review(browser, size, actor, action, case, operation):
    page = browser.new_page(viewport=size)
    page.set_default_timeout(20_000)
    try:
        token = "local-demo-" + actor
        sign_in(page, token, os.getenv("EKYC_STAFF_PORTAL_URL", PORTAL))
        intake = page.get_by_role("region", name="Investigation intake")
        item = intake.get_by_role("listitem").filter(has_text=operation)
        item.get_by_role("button", name="Acknowledge investigation").press("Enter")
        intake.get_by_label("Show").select_option("acknowledged")
        visible(item).to_contain_text("Acknowledged by " + actor)
        item.get_by_role("button", name="Inspect case").press("Enter")
        dialog = page.get_by_role("dialog", name="Case inspector")
        dialog.get_by_label("Disposition").select_option(action)
        dialog.get_by_label("Investigation reference").fill("synthetic-" + case)
        note = "Checked the original screening record.\n" + "ت" * 300
        dialog.get_by_label("Decision reason").fill(note)
        submit = dialog.get_by_role("button", name="Record disposition")
        status = dialog.locator("[role=status][aria-live=polite]")
        status.evaluate("""el => {
            window.reviewAnnouncements = [];
            new MutationObserver(() => window.reviewAnnouncements.push(el.textContent))
              .observe(el, {childList: true, characterData: true, subtree: true});
        }""")
        submit.focus()
        submit.press("Enter")
        announcement = "Verification " + ("approved" if action == "clear" else "rejected")
        visible(status).to_have_text(announcement, timeout=30_000)
        visible(dialog.get_by_role("heading", name="Loan application", exact=True)).to_be_focused()
        tree = page.context.new_cdp_session(page).send("Accessibility.getFullAXTree")["nodes"]
        assert any(
            n.get("role", {}).get("value") == "status"
            and any(
                p["name"] == "live" and p["value"]["value"] == "polite"
                for p in n.get("properties", [])
            )
            for n in tree
        )
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        handoff = wait_result(CASES + "/" + case + "/review", token, lambda v: bool(v["reviews"]))
        records = handoff["reviews"]
        assert [(r["actor"], r["operation_id"], r["reason"], r["action"]) for r in records] == [
            (actor, operation, note, action)
        ]
        assert page.evaluate("window.reviewAnnouncements").count(announcement) == 1
        page.keyboard.press("Escape")
        visible(dialog).to_have_count(0)
    finally:
        page.close()


def terminal_labels(browser, case_ids):
    for width, height in [(1280, 720), (375, 812)]:
        observer = browser.new_page(viewport={"width": width, "height": height})
        try:
            sign_in(observer, "local-demo-institution-observer")
            for case, label in case_ids:
                search = observer.get_by_label("Case or customer")
                search.fill(case)
                observer.get_by_role("button", name="Apply search").press("Enter")
                entry = observer.get_by_role("row").filter(
                    has=observer.get_by_role("link", name="Open case " + case)
                )
                visible(entry).to_contain_text(label)
                entry.get_by_role("link").press("Enter")
                dialog = observer.get_by_role("dialog")
                visible(dialog.locator(".ci-header")).to_contain_text(label)
                observer.keyboard.press("Escape")
        finally:
            observer.close()


def exercise_reference(browser):
    page = browser.new_page()
    try:
        page.goto(PORTAL + "/docs/#tag/service-status/GET/health/reads")
        page.get_by_role("button", name="Test Request (get /health/reads)").click()
        with page.expect_response(
            lambda r: urlsplit(r.url).path == "/docs-api/health/reads"
        ) as response:
            page.get_by_role("button", name=re.compile(r"^Send Request", re.I)).click()
        assert response.value.status == 200, response.value.text()
        spec = page.request.get(PORTAL + "/openapi.json").json()
        cases_url = PORTAL + "/docs-api/v2/cases"
        opening_doc = spec["paths"]["/v2/cases"]["post"]["requestBody"]["content"][
            "application/json"
        ]
        lending_headers = {"Authorization": "Bearer " + LENDING, "Idempotency-Key": new_key()}
        opened = page.request.post(
            cases_url, headers=lending_headers, data=opening_doc["examples"]["loan"]["value"]
        )
        assert opened.status == 202, opened.text()
        loan = wait_result(
            cases_url + "/" + opened.json()["case_id"], LENDING, lambda v: bool(v["policy_version"])
        )
        assert loan["product"] == "lending"

        subject = "guide-" + new_key()
        reference = issue(subject, "address", scenario="interactive")
        wallet_headers = {"Authorization": "Bearer " + WALLET, "Idempotency-Key": new_key()}
        opened = page.request.post(
            cases_url,
            headers=wallet_headers,
            data={
                "subject_ref": subject,
                "use_case": "address_refresh",
                "evidence": {"address": reference},
            },
        )
        assert opened.status == 202, opened.text()
        url = cases_url + "/" + opened.json()["case_id"]
        pending = wait_result(
            url + "/interactive",
            WALLET,
            lambda v: checks(v).get("address", {}).get("status") == "awaiting_response",
        )
        operation = checks(pending)["address"]["operation_id"]
        callback = spec["paths"]["/v2/cases/{id}/interactive/{kind}"]["post"]
        choices = callback["requestBody"]["content"]["application/json"]["schema"]["properties"][
            "response"
        ]
        sent = page.request.post(
            url + "/interactive/address?" + urlencode({"operation_id": operation}),
            headers=wallet_headers,
            data={"response": choices["examples"][0]},
        )
        assert sent.status in (200, 202), sent.text()
        final = wait_result(url, WALLET, lambda v: v["state"] == "APPROVED")
        assert checks(final)["address"]["operation_id"] == operation
    finally:
        page.close()


def verify_staff_deployment(browser, case):
    origin = os.getenv("EKYC_STAFF_PORTAL_URL")
    if not origin:
        return
    page = browser.new_page()
    try:
        page.goto(origin + "/#lab")
        visible(page.get_by_label("Personal access token")).to_be_visible()
        assert page.request.get(origin + "/portal-config").json() == {"mode": "staff"}
        assert page.request.post(origin + "/product-demo/lending/cases", data={}).status == 404
        token = "local-demo-institution-observer"
        sign_in(page, token, origin)
        visible(page.get_by_role("heading", name="Verification cases")).to_be_visible()
        result = page.request.post(
            origin + "/staff-api/v2/cases/" + case + "/review",
            headers={"Authorization": "Bearer " + token, "Idempotency-Key": new_key()},
            data={
                "check": "sanctions",
                "operation_id": case + "/sanctions/1",
                "disposition": "clear",
                "reason": "Synthetic permission check.",
                "review_ref": "observer-check",
            },
        )
        assert result.status == 403, result.text()
    finally:
        page.close()


def main():
    with sync_playwright() as driver:
        browser = driver.chromium.launch(
            executable_path=os.getenv("EKYC_BROWSER_EXECUTABLE") or None
        )
        try:
            exercise_reference(browser)
            case, op = start_case(browser)
            verify_staff_deployment(browser, case)
            review(browser, {"width": 1280, "height": 720}, "reviewer-alice", "clear", case, op)
            observer = browser.new_page()
            sign_in(observer, "local-demo-institution-observer")
            visible(observer.get_by_role("heading", name="Verification cases")).to_be_visible()
            visible(observer.get_by_role("link", name="Investigations")).to_have_count(0)
            observer.get_by_role("link", name="Overview").click()
            visible(
                observer.get_by_role("heading", name="Verification requirements")
            ).to_be_visible()
            observer.get_by_role("link", name="View all cases").click()
            live = observer.get_by_role("button", name="Return to live results")
            visible(live).to_be_visible()
            second, op = start_case(browser)
            observer.get_by_label("Case or customer").fill(second)
            observer.get_by_role("button", name="Apply search").press("Enter")
            visible(observer.get_by_text("0 matching cases", exact=True)).to_be_visible()
            live.press("Enter")
            row = observer.get_by_role("link", name="Open case " + second)
            visible(row).to_be_visible()
            row.press("Enter")
            visible(observer.get_by_role("dialog")).to_be_visible()
            visible(observer.get_by_role("button", name="Record disposition")).to_have_count(0)
            observer.close()
            review(browser, {"width": 375, "height": 812}, "reviewer-sam", "confirm", second, op)
            terminal_labels(
                browser, [(case, "Verification approved"), (second, "Verification rejected")]
            )
        finally:
            browser.close()
    print(
        "Browser verified two personal reviewer decisions, keyboard focus and live announcements at both widths, and the observer’s report cutoff."
    )


if __name__ == "__main__":
    main()
