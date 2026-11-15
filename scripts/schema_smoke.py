"""Check the served Liam schema against the application tables and exercise table search."""

import os
import re
from pathlib import Path
from urllib.parse import urlsplit

from playwright.sync_api import expect, sync_playwright


# export_schema --check compares this file byte-for-byte with the running catalog.
DDL = (Path(__file__).resolve().parents[1] / "apps/portal/schema/structure.sql").read_text()
COLUMN_COUNTS = {
    table: sum(1 for line in body.splitlines() if line.strip() and not line.lstrip().startswith("CONSTRAINT "))
    for table, body in re.findall(r"CREATE TABLE public\.(\w+) \(\n(.*?)\n\);", DDL, re.S)
}
FOREIGN_KEYS = [
    (table, tuple(cols.split(", ")), target, tuple(refs.split(", ")))
    for table, cols, target, refs in re.findall(
        r"ALTER TABLE ONLY public\.(\w+)\s+ADD CONSTRAINT \w+ FOREIGN KEY \(([^)]+)\) "
        r"REFERENCES public\.(\w+)\(([^)]+)\)", DDL
    )
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
        f"Liam displayed {len(COLUMN_COUNTS)} tables, {sum(COLUMN_COUNTS.values())} columns and {len(FOREIGN_KEYS)} foreign keys. Local search opened the outbox revision relationship."
    )
