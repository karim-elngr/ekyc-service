"""Verify public command errors, credential rotation and registration using a disposable Compose project."""
import argparse
import json
import os
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import URLError
from urllib.parse import quote

from http_support import API, expect, http, wait_result
from journey_support import CASES, LENDING, REVIEWER, checks, issue, new_key, post, read
from register import register

ROOT = Path(__file__).resolve().parents[1]


def until(url, status, timeout=70):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            actual, _, _ = http("GET", url)
            if actual == status:
                return
        except (URLError, OSError):
            pass
        time.sleep(.25)
    raise AssertionError(f"Timed out waiting for HTTP {status} at {url}")


def check_conflicts():
    key = new_key()
    subject = "contracts-" + key
    opening = {"use_case":"loan_application","subject_ref":subject}
    case = post(CASES, LENDING, opening, key)
    url = CASES + "/" + case

    def conflict(path, token, body, command_key, code):
        _, response = expect(http("POST", path, body, token, command_key), 409)
        assert response == {"error":code}, (response, code)

    conflict(CASES, LENDING, {**opening, "subject_ref":subject + "-other"}, key, "idempotency_key_reused")
    conflict(url + "/retry", LENDING, {"check":"address"}, new_key(), "invalid_case_state")
    doc = issue(subject, "document", screening_scenario="hit")
    post(url + "/evidence", LENDING, {"evidence":{"document":doc,
          "face":issue(subject,"face",document_ref=doc),"address":issue(subject,"address")}})
    view = read(case, LENDING, lambda v: v["state"] == "AWAITING_REVIEW")
    disposition = {"check":"sanctions", "disposition":"confirm","operation_id":"stale",
                   "reason":"Confirmed by the synthetic investigation.","review_ref":new_key()}
    conflict(url + "/review", REVIEWER, disposition, new_key(), "stale_operation")
    disposition["operation_id"] = checks(view)["sanctions"]["operation_id"]
    post(url + "/review", REVIEWER, disposition)
    read(case, LENDING, lambda v: v["state"] == "REJECTED")
    conflict(url + "/retry", LENDING, {"check":"address"}, new_key(), "case_already_decided")
    return case, opening, key


def stale_registrar():
    """A matching URI alone must never let an outdated service declaration pass the registrar."""
    worker_url = "http://synthetic-worker:9080"
    class Admin(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/deployments":
                data = {"deployments":[{"id":"dp-old","uri":worker_url}]}
            else:
                data = {"deployment_id":"dp-old","handlers":[{"name":"command","public":True}]}
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(data).encode())
        def log_message(self, *_):
            pass
    with ThreadingHTTPServer(("127.0.0.1",0), Admin) as admin:
        thread = threading.Thread(target=admin.serve_forever,daemon=True)
        thread.start()
        try:
            try:
                register(f"http://127.0.0.1:{admin.server_port}", worker_url, timeout=3)
            except SystemExit as exc:
                assert "Stale worker registration" in str(exc), exc
            else:
                raise AssertionError("Registrar accepted a matching URI missing readiness_v2")
        finally:
            admin.shutdown()
            thread.join()


def verify_independent_issuer(origin, api_origin, opening, caller_token, issuer_token):
    from contract_smoke import ISSUER_SPEC, checked, validate

    endpoint = origin + "/v2/resolve"
    subject = opening["subject_ref"]
    request = {"institution":"demo-bank", "subject_ref":subject, "evidence":{}}
    refs = {kind:"issued-" + kind for kind in ("document","face","address")}
    for body,status in [
        ({**request, "subject_ref":subject+"-new"}, 200),
        (request, 200),
        ({**request, "evidence":{kind:refs[kind] for kind in ("document","face")}},200),
        ({**request, "evidence":refs}, 200),
        ({**request, "evidence":refs, "subject_ref":subject+"-different"}, 422),
        ({**request, "subject_ref":subject+"-outage"}, 503),
    ]:
        validate(ISSUER_SPEC, {"$ref":"#/components/schemas/ResolveRequest"}, body)
        checked(ISSUER_SPEC, "/v2/resolve", "post", http("POST",endpoint,body,issuer_token), status)
    expect(http("POST", endpoint, request, "wrong-test-credential"),401)
    cases = api_origin + "/v2/cases"
    _, receipt = expect(http("POST",cases,{**opening,"evidence":refs},caller_token,new_key()),202)
    final = wait_result(cases+"/"+receipt["case_id"],caller_token,lambda view:view["state"]=="APPROVED",65)
    steps = checks(final)
    assert set(steps)=={"document","face","address","sanctions"}, steps
    assert all(step["status"]=="PASSED" and step["origin"]=="fresh" and step["operation_id"]
               for step in steps.values()), steps
    print("Independent institution issuer completed four fresh checks through the worker and all four adapters.")


class DrillContainers:
    def __init__(self, engine, project, override):
        self.engine = engine
        self.project = project
        self.compose = [engine,"compose","-p",project,"-f",str(ROOT/"compose.yaml")]
        if override:
            self.compose.extend(["-f",override])
        self.created = []

    @staticmethod
    def output(args, **options):
        result = subprocess.run(args,capture_output=True,text=True,timeout=90,**options)
        if result.returncode:
            raise RuntimeError(result.stderr.strip() or "Container command failed")
        return result.stdout.strip()

    def service(self, service):
        ids = self.output([*self.compose,"ps","-q",service]).splitlines()
        for candidate in ids:
            info = json.loads(self.output([self.engine,"inspect",candidate]))[0]
            if info["Config"]["Labels"].get("com.docker.compose.service") == service:
                return info
        raise RuntimeError(f"Couldn't find {service} in {self.project}")

    def start(self, name, network, image, ports, env=None, command=(), extra=()):
        args = [self.engine,"run","--detach","--name",name,"--network",network]
        for port in ports:
            args.extend(["--publish",f"127.0.0.1::{port}"])
        for key in env or {}:
            args.extend(["--env",key])
        self.output([*args,*extra,image,*command],env={**os.environ,**(env or {})})
        self.created.append(name)

    def origin(self, name, port):
        binding = self.output([self.engine,"port",name,f"{port}/tcp"]).splitlines()[0]
        return "http://" + binding

    def clean(self):
        for name in reversed(self.created):
            self.output([self.engine,"rm","--force","--volumes",name])


def vendor_switch(api, issuer, second_vendor, product_token, reviewer_token):
    subject = "switch-" + new_key()
    case_url = api + "/v2/cases"
    def asset(kind, **extra):
        _, value = expect(http("POST", issuer + "/demo/evidence", {
            "institution":"demo-bank", "subject_ref":subject, "kind":kind, **extra
        }, key=new_key()), 201)
        return value["evidence_ref"]

    doc = asset("document")
    address = asset("address")
    cases = {}
    for vendor in ["face","face-after"]:
        face = asset("face", document_ref=doc, provider=vendor)
        ref = post(case_url, product_token, {"subject_ref":subject,"use_case":"loan_application",
            "evidence":{"document":doc,"address":address,"face":face}})
        final = wait_result(case_url + "/" + ref, product_token, lambda v:v["state"]=="APPROVED")
        op = checks(final)["face"]["operation_id"]
        _, handoff = expect(http("GET", case_url + "/" + ref + "/review", token=reviewer_token),200)
        saved = checks(handoff)["face"]
        assert (saved["operation_id"],saved["provider"],saved["method"]) == (op,vendor,"face_comparison"),saved
        path = second_vendor + "/__mock__/operations/" + quote(op, safe="")
        status, _, result = http("GET",path)
        if vendor == "face-after":
            assert status == 200 and result["result_saved"], result
        else:
            assert status == 404 and result["error"] == "unknown_operation", result
        cases[vendor] = (ref, saved)
    ref, saved = cases["face"]
    _, original = expect(http("GET",case_url + "/" + ref + "/review",token=reviewer_token),200)
    assert checks(original)["face"] == saved


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine",required=True,help="Docker or Podman executable name or path")
    parser.add_argument("--project",required=True)
    parser.add_argument("--override")
    args = parser.parse_args()
    if args.project == "ekyc":
        parser.error("Use a disposable project distinct from the regular demo")
    stale_registrar()
    until(API + "/health/ready",200)
    case, opening, key = check_conflicts()
    stack = DrillContainers(args.engine,args.project,args.override)
    api, restate, worker, issuer, face = (stack.service(name) for name in ["api","restate","worker","issuer","face"])
    network = next(iter(api["NetworkSettings"]["Networks"]))
    endpoint = lambda x: dict(entry.split("=",1) for entry in x["Config"]["Env"] if "=" in entry)
    base = endpoint(api)
    creds = json.loads((ROOT/"config"/"demo-clients.json").read_text())
    rotated = "rotated-" + uuid.uuid4().hex
    grant = next(g for g in creds if g["access"]["role"]=="product" and g["access"]["product"]=="lending")
    grant["token"] = rotated
    reviewer = next(g for g in creds if g["access"]["role"]=="reviewer" and g["access"]["product"]=="lending")
    tag = uuid.uuid4().hex[:12]
    face_after = "ekyc-contract-face-" + tag
    unregistered = "ekyc-contract-restate-" + tag
    rotated_api = "ekyc-contract-api-" + tag
    fault_api = "ekyc-contract-fault-" + tag
    sibling_worker = "ekyc-contract-worker-" + tag
    sibling_issuer = "ekyc-contract-issuer-" + tag
    config = {"EKYC_CLIENTS":json.dumps([grant,reviewer]),"DATABASE_URL":base["DATABASE_URL"],
              "RESTATE_INGRESS_URL":base["RESTATE_INGRESS_URL"]}
    try:
        # Independent API process over the existing case store with only the replacement token.
        stack.start(rotated_api,network,api["Config"]["Image"],[8090],config,["ekyc-api"])
        rotated_origin = stack.origin(rotated_api,8090)
        until(rotated_origin + "/health/ready",200)
        url = rotated_origin + "/v2/cases/" + case
        expect(http("GET",url,token=LENDING),401)
        _, before = expect(http("GET",url,token=rotated),200)
        _, replay = expect(http("POST",rotated_origin+"/v2/cases",opening,rotated,key),202)
        assert replay["case_id"] == case
        assert expect(http("GET",url,token=rotated),200)[1] == before

        # The second Restate node shares no metadata or log volume with the project.
        stack.start(unregistered,network,restate["Config"]["Image"],[9070], {"RESTATE_ROCKSDB_TOTAL_MEMORY_SIZE": "256 MiB"})
        admin = stack.origin(unregistered,9070)
        until(admin + "/deployments",200)
        stack.start(fault_api, network, api["Config"]["Image"],[8090],
                    {**config,"RESTATE_INGRESS_URL":"http://" + unregistered + ":8080"},["ekyc-api"])
        fault_origin = stack.origin(fault_api,8090)
        until(fault_origin + "/health/reads",200)
        until(fault_origin + "/health/ready",503)
        expect(http("GET",fault_origin+"/v2/cases/"+case,token=rotated),200)
        payload = {**opening, "subject_ref":"registration-" + tag}
        _, error = expect(http("POST",fault_origin+"/v2/cases",payload,rotated,new_key()),503)
        assert error["error"] == "dependency_unavailable", error
        worker_env = {k:v for k,v in endpoint(worker).items()
                      if k.endswith("_URL") or k in ("EKYC_EVIDENCE_TOKEN","EKYC_PROVIDER_CATALOG")}
        stack.start(face_after, network,face["Config"]["Image"],[8080],command=["face"])
        catalog = {kind:{kind:worker_env[kind.upper()+"_URL"]} for kind in ("document","face","address","sanctions")}
        catalog["face"]["face-after"] = "http://" + face_after + ":8080"
        worker_env["EKYC_PROVIDER_CATALOG"] = json.dumps(catalog)
        worker_env["RESTATE_ADMIN_URL"] = "http://" + unregistered + ":9070"
        worker_env["EKYC_EVIDENCE_URL"] = "http://" + sibling_issuer + ":9805"
        stack.start(sibling_worker,network,worker["Config"]["Image"],[],worker_env,["ekyc-worker"])
        register(admin,"http://" + sibling_worker + ":9080")
        # The worker is registered, but its own real issuer has not started.
        until(fault_origin + "/health/ready",503)
        until(fault_origin + "/health/reads",200)
        expect(http("POST",fault_origin+"/v2/cases",payload,rotated,new_key()),503)
        # Run a second implementation of the public issuer contract, with no demo issuer code or database.
        token = worker_env["EKYC_EVIDENCE_TOKEN"]
        issuer_env = {"ISSUER_BIND":"0.0.0.0", "ISSUER_SUBJECT":payload["subject_ref"],
                      "EKYC_EVIDENCE_TOKEN":token}
        issuer_source = (ROOT / "scripts" / "issuer_interop.py").read_text()
        stack.start(sibling_issuer, network, issuer["Config"]["Image"], [9805], issuer_env,
                    ["python3", "-u", "-c", issuer_source])
        until(fault_origin + "/health/ready",200)
        verify_independent_issuer(stack.origin(sibling_issuer,9805), fault_origin, payload, rotated, token)

        # Keep the independently implemented issuer and its worker intact. New cases
        # now use a second deployment whose demo issuer can select between face vendors.
        switch_issuer = sibling_issuer + "-switch"
        switch_worker = sibling_worker + "-switch"
        demo_env = {"PORT": "9805", "ISSUER_BIND": "0.0.0.0", "ISSUER_DB": "/tmp/issuer.sqlite3",
                    "EKYC_EVIDENCE_TOKEN": worker_env["EKYC_EVIDENCE_TOKEN"]}
        stack.start(switch_issuer, network, issuer["Config"]["Image"], [9805],
                    demo_env, issuer["Config"]["Cmd"], ["--volumes-from", issuer["Id"] + ":ro"])
        switch_config = {**worker_env, "EKYC_EVIDENCE_URL": "http://" + switch_issuer + ":9805"}
        stack.start(switch_worker, network, worker["Config"]["Image"], [], switch_config, ["ekyc-worker"])
        register(admin, "http://" + switch_worker + ":9080")
        vendor_switch(fault_origin, stack.origin(switch_issuer, 9805),
                      stack.origin(face_after, 8080), rotated, reviewer["token"])

        # Exercise actual production startup on the same DB and provider configuration.
        env = {k:v for k,v in endpoint(worker).items() if k.endswith("_URL") or k=="EKYC_PROVIDER_CATALOG"}
        env.pop("EKYC_EVIDENCE_URL",None)
        result = subprocess.run([args.engine,"run","--rm","--network",network,
            *[item for key in env for item in ("--env",key)],worker["Config"]["Image"],"ekyc-worker"],
            capture_output=True,text=True,timeout=30,env={**os.environ,**env})
        assert result.returncode != 0 and "EKYC_EVIDENCE" in result.stderr, result.stderr
    finally:
        stack.clean()
    print("Verified four distinct HTTP conflicts, stable caller-ID token rotation, stale registration rejection, "
          "unregistered/read-only availability, issuer-down and restored command routing, mandatory issuer startup settings, and two actual face vendors with historical source attribution.")


if __name__ == "__main__":
    main()
