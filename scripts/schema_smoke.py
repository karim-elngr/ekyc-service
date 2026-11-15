"""Check the served Liam schema against the application tables and exercise table search."""

import os
from urllib.parse import urlsplit

from playwright.sync_api import expect, sync_playwright


COLUMN_COUNTS = {
    "caller_quota": 3,
    "decision_outbox": 9,
    "product_case": 9,
    "product_case_revision": 6,
    "product_fact": 6,
    "provider_gate": 5,
    "provider_lease": 7,
    "staff_work_acknowledgement": 7,
    "subject_fact_generation": 3,
}

FOREIGN_KEYS = [
    (
        "decision_outbox",
        ("product_case_id", "version"),
        "product_case_revision",
        ("case_id", "version"),
    ),
    ("product_case_revision", ("case_id",), "product_case", ("case_id",)),
    ("product_fact", ("origin_case_id",), "product_case", ("case_id",)),
    ("provider_lease", ("provider",), "provider_gate", ("provider",)),
    ("staff_work_acknowledgement", ("case_id",), "product_case", ("case_id",)),
]


def check_schema(schema):
    tables = schema["tables"]
    counts = {name: len(table["columns"]) for name, table in tables.items()}
    assert counts == COLUMN_COUNTS, counts
    foreign_keys = [
        (name, tuple(key["columnNames"]), key["targetTableName"], tuple(key["targetColumnNames"]))
        for name, table in tables.items()
        for key in table["constraints"].values()
        if key["type"] == "FOREIGN KEY"
    ]
    assert sorted(foreign_keys) == sorted(FOREIGN_KEYS), foreign_keys


def verify_browser(browser, portal):
    url = portal.rstrip("/") + "/schema/"
    schema_origin = urlsplit(url)
    external, errors = [], []
    context = browser.new_context(service_workers="block")

    def local_requests(route):
        request_url = urlsplit(route.request.url)
        if request_url.scheme in {"http", "https"} and (request_url.scheme, request_url.netloc) != (
            schema_origin.scheme,
            schema_origin.netloc,
        ):
            external.append(route.request.url)
            route.abort()
        else:
            route.continue_()

    def track_response(response):
        if response.status >= 400:
            errors.append(f"{response.status} {response.url}")

    try:
        context.route("**/*", local_requests)
        page = context.new_page()
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.on("response", track_response)
        with page.expect_response(lambda response: response.url == url + "schema.json") as response:
            page.goto(url)
        assert response.value.ok, response.value.status
        check_schema(response.value.json())

        for table in COLUMN_COUNTS:
            expect(
                page.get_by_role("button", name=f"Menu button for {table}", exact=True)
            ).to_be_visible()
        page.get_by_role("button", name="Open command palette to search features").click()
        page.get_by_role("combobox").fill("decision_outbox")
        page.get_by_role("option", name="decision_outbox", exact=True).click()
        expect(page.get_by_role("heading", name="decision_outbox", exact=True)).to_be_visible()
        expect(page.get_by_role("heading", name="Foreign key", exact=True)).to_be_visible()
        expect(page.get_by_text("product_case_id, version", exact=True)).to_be_visible()
        expect(
            page.get_by_text("product_case_revision (case_id, version)", exact=True)
        ).to_be_visible()
        expect(page.get_by_role("heading", name="Related tables", exact=True)).to_be_visible()
        page.evaluate("document.fonts.ready.then(() => true)")
        assert not external, f"Schema viewer requested external resources: {external}"
        assert not errors, errors
    finally:
        context.close()


if __name__ == "__main__":
    with sync_playwright() as driver:
        browser = driver.chromium.launch(
            executable_path=os.getenv("EKYC_BROWSER_EXECUTABLE") or None
        )
        try:
            verify_browser(browser, os.getenv("EKYC_PORTAL_URL", "http://127.0.0.1:5173"))
        finally:
            browser.close()
    print(
        "Liam displayed 9 tables, 55 columns and 5 foreign keys. Local table search opened the outbox and its two-column revision relationship."
    )
