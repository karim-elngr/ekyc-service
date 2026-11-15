"""Rehearse both talk routes, the schema diagram and a live suspended verification."""

import argparse
import os
import re
from pathlib import Path
from urllib.parse import urlencode

from playwright.sync_api import expect, sync_playwright

from deck_sources import DECK, synchronize


def shows(page, slide):
    expect(page.locator(f"#{slide}.slide")).to_be_visible()


def slide_bounds(page, slide_id, width):
    problems = page.locator(f"#{slide_id}").evaluate("""slide => {
        const footer = document.querySelector('.nav').getBoundingClientRect();
        const selector = 'h1,h2,h3,p,li,b,small,button,a,.route,.source,.annot,.bottomline,.opslist,.choice';
        return [...slide.querySelectorAll(selector)].filter(element => {
            const rect = element.getBoundingClientRect();
            return rect.width && (rect.left < -1 || rect.right > innerWidth + 1 ||
                (innerWidth > 720 && rect.bottom > footer.top + 1));
        }).map(element => element.textContent.trim().slice(0, 65));
    }""")
    assert not problems, (width, slide_id, problems)


def inspect_layout(page, slide_ids):
    for width, height in [(1280, 720), (375, 812)]:
        page.set_viewport_size({"width": width, "height": height})
        page.keyboard.press("Home")
        for slide_id in slide_ids:
            shows(page, slide_id)
            slide_bounds(page, slide_id, width)
            page.keyboard.press("ArrowRight")
    page.set_viewport_size({"width": 1280, "height": 720})


def advance_to(page, slide_id):
    # Bound the walk so a route that skips the expected slide fails immediately.
    for _ in range(page.locator(".slide").count()):
        if page.locator(f"#{slide_id}").is_visible():
            return
        page.keyboard.press("ArrowRight")
    raise AssertionError(f"The presenter route did not reach {slide_id}")


def check_hosted_source(link, path):
    expect(link).to_have_attribute("href", re.compile(r"^blob:"))
    document = link.evaluate("async link => (await fetch(link.href)).text()")
    # The hosted reader prepends a filename and line numbers to the checkout source.
    heading, _, numbered_source = document.partition("\n\n")
    assert heading == path
    source = re.sub(r"^ *\d+  ", "", numbered_source, flags=re.MULTILINE)
    assert source == (DECK.parents[1] / path).read_text()


def inspect_reliability_sources(page):
    path = "adapters/postgres/migrations/0002_dispatch_limits.sql"
    for slide, decisions in {
        "limits": ("active_count >= p_max_in_flight", "recent_count >= p_per_minute"),
        "breaker": (
            "active_probe_expiry IS NOT NULL",
            "finished.probe OR gate.failures + 1 >= p_threshold",
        ),
    }.items():
        advance_to(page, slide)
        figure = page.locator(f"#{slide} figure.source")
        code = figure.locator("pre code")
        expect(code).to_be_visible()
        for condition in decisions:
            expect(code).to_contain_text(condition)
        link = figure.get_by_role("link")
        if page.url.startswith("file:"):
            assert link.evaluate("a => a.href") == (DECK.parents[1] / path).as_uri()
        else:
            check_hosted_source(link, path)


def capture_gallery(page):
    trigger = page.locator("#livewait button[data-open-capture]")
    expect(trigger).to_be_visible()
    expect(page.locator("#livewait")).to_contain_text(
        re.compile(r"recorded|captured|rehearsal", re.I)
    )
    trigger.click()
    gallery = page.locator("#screens")
    expect(gallery).to_be_visible()
    expect(page.locator("#prevShot")).to_be_disabled()
    # Decoding each stored image also exercises the offline fallback without the demo.
    images = []
    for index in range(3):
        image = page.locator("#shotImage")
        image.evaluate("img => img.decode()")
        assert image.evaluate("img => img.complete && img.naturalWidth >= 800")
        assert image.get_attribute("alt")
        images.append(image.get_attribute("src"))
        expect(page.locator("#shotCount")).to_contain_text(f"{index + 1} / 3")
        if index < 2:
            page.keyboard.press("ArrowRight")
    assert len(set(images)) == 3 and all(src.startswith("data:image/") for src in images)
    expect(page.locator("#nextShot")).to_be_disabled()
    page.locator("#zoomShot").click()
    expect(page.locator("#zoomShot")).to_have_attribute("aria-pressed", "true")
    page.locator("#prevShot").click()
    expect(page.locator("#shotCount")).to_contain_text("2 / 3")
    page.keyboard.press("Escape")
    expect(gallery).not_to_be_visible()


def controls(page):
    ids = page.locator(".slide").evaluate_all("slides => slides.map(s => s.id)")
    assert ids[:4] == ["opening", "patterns", "architecture", "wallet"], ids
    assert ids[ids.index("dispatch") : ids.index("livewait") + 1] == [
        "dispatch",
        "limits",
        "breaker",
        "callback",
        "livewait",
    ]
    assert (
        ids.index("livewait")
        < ids.index("replay")
        < ids.index("recovery")
        < ids.index("late")
        < ids.index("outbox")
    )
    for slide_id, details in {
        "limits": [
            "60 commands per calendar minute",
            "Retry-After",
            "4 concurrent",
            "rolling minute",
        ],
        "breaker": ["2.5-second", "30-second", "3 attempts", "jitter", "5 seconds", "one probe"],
        "late": [
            "original operation",
            "One investigation alert",
            "approval stays fixed",
            "five minutes",
        ],
    }.items():
        for detail in details:
            expect(page.locator(f"#{slide_id}")).to_contain_text(re.compile(detail, re.I))

    expect(page.locator(".slide [data-sim], .slide .advanceSim, .slide .resetSim")).to_have_count(0)
    staged_actions = re.compile(
        r"^(Open loan|Start wallet|Submit evidence|Interrupt worker|Resume case|Next step)(?:\s*→)?$",
        re.I,
    )
    assert not any(
        staged_actions.match(label.strip())
        for label in page.locator(".slide button").all_text_contents()
    )
    assert (
        "illustrative walkthrough"
        not in " ".join(page.locator(".slide").all_text_contents()).lower()
    )
    inspect_layout(page, ids)

    page.keyboard.press("Home")
    page.get_by_role("button", name="Index", exact=False).first.click()
    live_title = page.locator("#livewait").get_attribute("data-title")
    page.locator(".index-link").filter(has_text=live_title).click()
    shows(page, "livewait")
    capture_gallery(page)
    page.keyboard.press("n")
    with page.expect_popup() as popup:
        page.get_by_role("button", name="Open presenter window").click()
    presenter = popup.value
    presenter.locator("#speaker-forward").click()
    shows(page, "replay")
    expect(presenter.locator("#speaker-title")).to_have_text(
        page.locator("#replay").get_attribute("data-title")
    )
    presenter.close()

    page.locator(".nav .open-setup").click()
    total = page.locator("#agenda tr").filter(
        has=page.get_by_role("cell", name="Talk total", exact=True)
    )
    expect(total.get_by_role("cell")).to_have_text(["Talk total", "30 min", "40 min", "45 min"])
    discussion = page.locator("#agenda tr").filter(
        has=page.get_by_role("cell", name="Design review", exact=True)
    )
    expect(discussion.get_by_role("cell")).to_have_text(
        ["Design review", "15 min", "15 min", "15 min"]
    )
    page.locator("#talkRoute").select_option("short")
    page.locator("#closeSetup").click()
    page.keyboard.press("Home")
    for slide_id in [slide_id for slide_id in ids if slide_id != "patterns"]:
        shows(page, slide_id)
        page.keyboard.press("ArrowRight")
    page.locator(".nav .open-setup").click()
    page.locator("#talkRoute").select_option("full")
    page.locator("#closeSetup").click()
    page.keyboard.press("Home")


def open_schema(deck, portal):
    shows(deck, "architecture")
    with deck.expect_popup() as opened:
        deck.locator("#architecture .schema-link").click()
    schema = opened.value
    try:
        expect(schema).to_have_url(portal + "/schema/")
        for name in ("product_case_revision", "decision_outbox"):
            expect(
                schema.get_by_role("button", name=f"Menu button for {name}", exact=True)
            ).to_be_visible(timeout=30000)
    finally:
        schema.close()
    deck.bring_to_front()


def data(lab, product, case_id):
    response = lab.request.get(
        f"{lab.url.split('/#')[0].rstrip('/')}/product-demo/{product}/cases/{case_id}"
    )
    assert response.ok, (response.status, case_id)
    return response.json()


def journey(deck, portal, admin, pictures=None):
    deck.keyboard.press("Home")
    advance_to(deck, "architecture")
    open_schema(deck, portal)
    advance_to(deck, "wallet")
    with deck.expect_popup() as new_lab:
        deck.locator("#wallet .live-link").click()
    lab = new_lab.value
    lab.set_default_timeout(45000)
    lab.locator("#lab-scenario").select_option("reuse")
    lab.get_by_role("button", name="Start case").click()
    lab.get_by_role("button", name="Open loan case").click()
    loan = lab.locator(".ci-case").filter(
        has=lab.get_by_role("heading", name="Loan application", exact=True)
    )
    wallet = lab.locator(".ci-case").filter(
        has=lab.get_by_role("heading", name="Wallet onboarding", exact=True)
    )
    expect(loan.locator(".ci-id code")).to_be_visible()
    wallet_id, loan_id = (x.locator(".ci-id code").text_content().strip() for x in (wallet, loan))
    assert wallet_id != loan_id
    first, second = data(lab, "wallet", wallet_id), data(lab, "lending", loan_id)
    assert first["subject_ref"] == second["subject_ref"]
    assert {
        c["kind"]: c["origin_case_id"] for c in second["checks"] if c["origin"] == "reused"
    } == {"document": wallet_id, "face": wallet_id}

    deck.bring_to_front()
    advance_to(deck, "livewait")
    tab_count = len(deck.context.pages)
    deck.locator("#livewait .live-link").first.click()
    assert len(deck.context.pages) == tab_count
    lab.bring_to_front()
    loan.get_by_label("Proof of address evidence").select_option("interactive")
    loan.get_by_role("button", name="Submit proof of address").click()
    send = loan.get_by_role("button", name="Send response")
    expect(send).to_be_visible(timeout=45000)
    if pictures:
        pictures.mkdir(parents=True, exist_ok=True)
        loan.locator(".ci-live").scroll_into_view_if_needed()
        lab.screenshot(path=str(pictures / "0.png"))
    invocation_link = loan.get_by_role("link", name="Restate invocation")
    invocation_id = invocation_link.get_attribute("href").rsplit("/", 1)[-1]
    assert invocation_id.startswith("inv_"), invocation_id
    with lab.expect_popup() as opened:
        invocation_link.click()
    journal = opened.value
    journal.wait_for_url(admin.rstrip("/") + "/ui/invocations/" + invocation_id, timeout=30000)
    journal.wait_for_load_state("domcontentloaded")
    journal_tab = journal.get_by_text("Journal", exact=True).first
    if journal_tab.is_visible():
        journal_tab.click()
    journal.get_by_text("awakeable", exact=False).first.wait_for(state="visible", timeout=30000)
    if pictures:
        journal.screenshot(path=str(pictures / "1.png"))
    lab.bring_to_front()
    send.click()
    expect(loan.locator(".badge.approved").first).to_have_text(
        "Verification approved", timeout=45000
    )
    final = data(lab, "lending", loan_id)
    assert all(c["status"] == "PASSED" for c in final["checks"])
    assert {c["kind"] for c in final["checks"] if c["origin"] == "fresh"} == {
        "address",
        "sanctions",
    }
    journal.reload()
    journal.get_by_text("Succeeded", exact=False).first.wait_for(state="visible", timeout=30000)
    if pictures:
        journal.screenshot(path=str(pictures / "2.png"))
    deck.bring_to_front()
    shows(deck, "livewait")
    deck.keyboard.press("ArrowRight")
    shows(deck, "replay")
    journal.close()
    lab.close()
    print(
        "Deck rehearsal passed. Both talk routes and the recording work offline. "
        "The hosted deck opened Liam and the real case lab. The Restate journal showed "
        "the pending awakeable and successful callback. The loan case reused the wallet's "
        "document and face checks and passed its own address and sanctions checks."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", type=Path, help="Save three local rehearsal screenshots")
    args = parser.parse_args()
    original, synced = synchronize()
    assert original == synced, (
        "Run python3 scripts/deck_sources.py --write to refresh quoted source"
    )
    portal = os.environ.get("EKYC_PORTAL_URL", "http://127.0.0.1:5173").rstrip("/")
    admin = os.environ.get("EKYC_RESTATE_ADMIN_URL") or os.environ.get(
        "RESTATE_ADMIN_URL", "http://127.0.0.1:9070"
    )
    query = urlencode({"portal": portal, "admin": admin})
    with sync_playwright() as pw:
        browser = pw.chromium.launch(executable_path=os.environ.get("EKYC_BROWSER_EXECUTABLE"))
        page = browser.new_page(viewport={"width": 1280, "height": 720})
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.goto(DECK.as_uri() + "?" + query)
        controls(page)
        inspect_reliability_sources(page)
        # A presenter can open the hosted talk without supplying local ports.
        page.goto(portal + "/talk/")
        inspect_reliability_sources(page)
        advance_to(page, "livewait")
        restate_link = page.locator("#livewait").get_by_role("link", name="Restate UI")
        admin_ui = admin.rstrip("/") + "/ui/"
        expect(restate_link).to_have_attribute("href", portal + "/invocations/")
        redirect = page.request.get(portal + "/invocations/", max_redirects=0)
        assert redirect.status in (301, 302, 303, 307, 308)
        assert redirect.headers["location"] == admin_ui
        with page.expect_popup() as opened:
            restate_link.click()
        restate = opened.value
        expect(restate).to_have_url(re.compile("^" + re.escape(admin_ui)), timeout=30000)
        restate.close()
        page.goto(portal + "/talk/?" + query)
        shows(page, "opening")
        expect(
            page.locator("#livewait").get_by_role("link", name="Restate UI", include_hidden=True)
        ).to_have_attribute("href", admin_ui)
        recovery_link = page.locator("#recovery").get_by_role(
            "link", name="Inspect the recovery assertions", include_hidden=True
        )
        check_hosted_source(recovery_link, "scripts/recovery_smoke.py")
        journey(page, portal, admin, args.capture)
        assert not errors, errors
        browser.close()
