"""Build and exercise this checkout in a uniquely named local Compose project."""

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from contextlib import ExitStack
from pathlib import Path
from urllib.error import URLError
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
OVERRIDE = ROOT / "scripts/compose.verify.yaml"
HOST_PORTS = (
    "API",
    "PORTAL",
    "STAFF_PORTAL",
    "ISSUER",
    "SINK",
    "RESTATE_ADMIN",
    "PG",
    "DOCUMENT",
    "FACE",
    "ADDRESS",
    "SANCTIONS",
)
IMAGES = {"SERVICE": ROOT, "MOCK": ROOT / "mocks", "PORTAL": ROOT / "apps/portal"}


def available_ports():
    # Hold all reservations at once so the OS returns distinct ports.
    with ExitStack() as reserved:
        ports = {}
        for name in HOST_PORTS:
            listener = reserved.enter_context(socket.socket())
            listener.bind(("127.0.0.1", 0))
            ports[name] = listener.getsockname()[1]
        return ports


def wait_for(url, timeout=90):
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        try:
            with urlopen(url, timeout=4) as response:
                if response.status == 200:
                    return
        except (URLError, OSError) as error:
            last = error
        time.sleep(0.5)
    raise RuntimeError(f"{url} unavailable after {timeout}s: {last}")


def main():
    engine = os.environ.get("CONTAINER_ENGINE") or shutil.which("docker") or shutil.which("podman")
    if not engine:
        raise SystemExit("Install Docker Compose or Podman with a Compose provider.")
    # Fail early if the selected Python has no release dependencies.
    import jsonschema  # noqa: F401
    import playwright.sync_api  # noqa: F401

    env = os.environ.copy()
    project = "ekyc-verify-" + uuid.uuid4().hex[:12]
    ports = available_ports()
    for name, port in ports.items():
        env["EKYC_" + name + "_PORT"] = str(port)
        if name in ("DOCUMENT", "FACE", "ADDRESS", "SANCTIONS"):
            env[name + "_URL"] = f"http://127.0.0.1:{port}"
        else:
            env[f"EKYC_{name}_URL"] = f"http://127.0.0.1:{port}"
    env["DATABASE_URL"] = f"postgres://ekyc:local-demo-only@127.0.0.1:{ports['PG']}/ekyc"
    compose = [
        engine,
        "compose",
        "-p",
        project,
        "--profile",
        "portal",
        "-f",
        str(ROOT / "compose.yaml"),
        "-f",
        str(OVERRIDE),
    ]

    def run(*args, check=True):
        print("+", " ".join(map(str, args)), flush=True)
        return subprocess.run(args, env=env, cwd=ROOT, check=check)

    def script(name, *args):
        run(sys.executable, ROOT / "scripts" / (name + ".py"), *args)

    images = []
    try:
        run("cargo", "fmt", "--all", "--", "--check")
        run(
            "cargo",
            "clippy",
            "--locked",
            "--workspace",
            "--all-features",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        )
        run(sys.executable, "-m", "unittest", "discover", "-s", "mocks", "-v")
        for role, context in IMAGES.items():
            tag = f"localhost/{project}-{role.lower()}:dev"
            images.append(tag)
            env["EKYC_" + role + "_IMAGE"] = tag
            flags = ["--layers"] if Path(engine).name == "podman" else []
            run(engine, "build", *flags, "-t", tag, str(context))
        run(*compose, "up", "--no-build", "-d")
        wait_for(env["EKYC_API_URL"] + "/health/ready")
        wait_for(env["EKYC_PORTAL_URL"] + "/")
        wait_for(env["EKYC_STAFF_PORTAL_URL"] + "/portal-config")
        script("export_schema", "--engine", engine, "--project", project, "--check")
        run("cargo", "test", "--locked", "--workspace", "--all-features")
        run(engine, "run", "--rm", env["EKYC_PORTAL_IMAGE"], "pnpm", "test")
        for name in (
            "first_case",
            "product_smoke",
            "queries_smoke",
            "interactive_smoke",
            "binding_drift_smoke",
            "contract_smoke",
            "late_screen_smoke",
            "reconciliation_smoke",
            "provider_ordering_smoke",
            "browser_smoke",
            "deck_smoke",
            "schema_smoke",
        ):
            script(name)
        isolated = ("--engine", engine, "--project", project, "--override", str(OVERRIDE))
        script("service_contract_drills", *isolated)
        script("legacy_delivery_smoke", *isolated)
        script("sanctions_restart_smoke", *isolated)
        with tempfile.TemporaryDirectory() as scratch:
            receipt = Path(scratch) / "recovery.json"
            script("recovery_smoke", "prepare", receipt)
            run(*compose, "kill", "-s", "KILL", "worker")
            run(*compose, "start", "worker")
            script("recovery_smoke", "verify", receipt)
        wait_for(env["EKYC_API_URL"] + "/health/ready")
    finally:
        if len(images) == len(IMAGES):
            run(*compose, "down", "--volumes", "--remove-orphans")
        for image in images:
            run(engine, "image", "rm", image, check=False)
    print(
        "Release checks passed, including the worker restart and both browser journeys.", flush=True
    )


if __name__ == "__main__":
    main()
