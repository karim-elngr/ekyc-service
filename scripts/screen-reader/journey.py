"""Exercise the built portal with native Orca and two deterministic case transitions."""

import json
import threading
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from selenium import webdriver
from selenium.webdriver.chrome.service import Service
from selenium.webdriver.common.by import By
from selenium.webdriver.common.keys import Keys
from selenium.webdriver.support.select import Select
from selenium.webdriver.support.ui import WebDriverWait


WALLET = "v2~wallet~" + "a" * 64
LOAN = "v2~lending~" + "b" * 64
OPENED = "2026-11-15T20:31:58Z"
completed = {WALLET: False, LOAN: False}
reads = {WALLET: 0, LOAN: 0}


def case_view(case):
    done = completed[case]
    if case == LOAN:
        product, use_case, kind = "lending", "loan_application", "sanctions"
        state, status, action = (
            ("REJECTED", "FAILED", None)
            if done
            else ("AWAITING_REVIEW", "REVIEW", "review")
        )
    else:
        product, use_case, kind = "wallet", "address_refresh", "address"
        state, status, action = (
            ("APPROVED", "PASSED", None)
            if done
            else ("AWAITING_EVIDENCE", "AWAITING_EVIDENCE", "provide_evidence")
        )
    return {
        "case_id": case,
        "subject_ref": "synthetic",
        "product": product,
        "use_case": use_case,
        "policy_version": "v1",
        "version": 2 if done else 1,
        "state": state,
        "created_at": OPENED,
        "checks": [
            {
                "kind": kind,
                "status": status,
                "operation_id": "op-1",
                "reasons": [],
                "next_action": action,
            }
        ],
    }


class Portal(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory="/portal", **kwargs)

    def log_message(self, *_):
        pass

    def reply(self, result):
        data = json.dumps(result).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/portal-config":
            return self.reply({"mode": "demo"})
        if path == "/staff-api/v2/staff/session":
            return self.reply(
                {
                    "actor_id": "synthetic",
                    "institution": "test",
                    "products": ["lending"],
                    "role": "reviewer",
                }
            )
        if path == "/staff-api/v2/investigations":
            item = {
                "work_id": "w1",
                "case_id": LOAN,
                "kind": "screening_review",
                "product": "lending",
                "state": "AWAITING_REVIEW",
                "operation_id": "op-1",
                "opened_at": OPENED,
            }
            return self.reply(
                {"items": [] if completed[LOAN] else [item], "next_after": None}
            )
        for case in completed:
            if path.endswith("/" + case + "/review"):
                return self.reply(
                    {"checks": case_view(case)["checks"], "reviews": [], "alerts": []}
                )
            if path.endswith("/" + case):
                reads[case] += 1
                return self.reply(case_view(case))
        return super().do_GET()

    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        if self.path == "/product-demo/evidence":
            return self.reply({"evidence_ref": "synthetic"})
        if self.path.endswith("/cases"):
            return self.reply({"case_id": WALLET})
        for case, action in [(WALLET, "evidence"), (LOAN, "review")]:
            if self.path.endswith("/" + case + "/" + action):
                completed[case] = True
                return self.reply({"case_id": case})
        self.send_error(404)


def utterances(offset):
    with Path("/tmp/orca.log").open("rb") as log:
        log.seek(offset)
        return [
            line.partition("SPEECH OUTPUT: ")[2]
            for line in log.read().decode(errors="replace").splitlines()
            if "SPEECH OUTPUT: " in line
        ]


def journey(page, url, case):
    wait = WebDriverWait(page, 15)

    def element(selector):
        return wait.until(lambda driver: driver.find_element(By.CSS_SELECTOR, selector))

    def button(name):
        return wait.until(
            lambda driver: driver.find_element(
                By.XPATH, f"//button[normalize-space()='{name}']"
            )
        )

    if case == LOAN:
        page.get(url + "/#staff")
        element("input[type=password]").send_keys("synthetic")
        button("Sign in").send_keys(Keys.ENTER)
        button("Inspect case").send_keys(Keys.ENTER)
        Select(element(".ci-disposition select")).select_by_value("confirm")
        element(".ci-disposition input").send_keys("test-ref")
        element(".ci-disposition textarea").send_keys(
            "Synthetic screening check complete."
        )
        submit, title, expected = (
            button("Record disposition"),
            "Loan application",
            "Verification rejected",
        )
    else:
        page.get(url + "/#lab")
        Select(element("#lab-scenario")).select_by_value("correction")
        button("Start case").send_keys(Keys.ENTER)
        submit, title, expected = (
            button("Submit proof of address"),
            "Address refresh",
            "Approved",
        )

    page.execute_script("arguments[0].focus()", submit)
    time.sleep(1.5)
    start = Path("/tmp/orca.log").stat().st_size
    submit.send_keys(Keys.ENTER)
    wait.until(
        lambda driver: driver.switch_to.active_element.tag_name == "h2"
        and driver.switch_to.active_element.text == title
    )
    wait.until(
        lambda _: any(expected in phrase for phrase in utterances(start)),
        f"Orca did not announce {expected}.",
    )
    time.sleep(0.7)  # Allow speech accompanying the focus change to settle.
    heard = utterances(start)
    count = sum(expected in phrase for phrase in heard)
    assert count == 1, (expected, heard)
    prior_reads = reads[case]
    wait.until(lambda _: reads[case] >= prior_reads + 2)
    time.sleep(0.7)
    assert utterances(start) == heard, "A fresh poll produced extra speech"
    print(
        f"Focus returned to the {title} heading. Orca spoke {expected!r} once. "
        f"{reads[case] - prior_reads} case polls were silent."
    )


if __name__ == "__main__":
    options = webdriver.ChromeOptions()
    for flag in (
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--disable-background-networking",
        "--disable-gpu",
        "--force-renderer-accessibility=complete",
        "--no-first-run",
        "--window-size=1280,850",
    ):
        options.add_argument(flag)
    with ThreadingHTTPServer(("127.0.0.1", 0), Portal) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with webdriver.Chrome(
                service=Service("/usr/bin/chromedriver"), options=options
            ) as browser:
                origin = f"http://127.0.0.1:{server.server_port}"
                for case in (WALLET, LOAN):
                    journey(browser, origin, case)
        finally:
            server.shutdown()
            thread.join()
