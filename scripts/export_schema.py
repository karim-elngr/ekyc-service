"""Refresh the Liam ERD source from the running demo's PostgreSQL catalog."""

import argparse
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / "apps/portal/schema/structure.sql"


def export_schema(engine, project):
    command = [engine, "compose", "-f", str(ROOT / "compose.yaml")]
    if project:
        command.extend(["-p", project])
    command.extend(
        [
            "exec",
            "-T",
            "postgres",
            "pg_dump",
            "--username=ekyc",
            "--dbname=ekyc",
            "--schema-only",
            "--schema=public",
            "--exclude-table=public._sqlx_migrations",
            "--no-owner",
            "--no-privileges",
            "--no-comments",
            "--encoding=UTF8",
        ]
    )
    try:
        dump = subprocess.run(
            command, check=True, stdout=subprocess.PIPE, encoding="utf-8", cwd=ROOT
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        raise SystemExit(f"Could not read the running postgres service: {exc}") from exc

    # pg_dump 17+ wraps plain dumps in randomly keyed psql commands. The ERD
    # parser takes SQL; client versions and changing keys also obscure schema diffs.
    metadata = (
        "\\restrict ",
        "\\unrestrict ",
        "-- Dumped from database version ",
        "-- Dumped by pg_dump version ",
    )
    return (
        "\n".join(line for line in dump.splitlines() if not line.startswith(metadata)).rstrip()
        + "\n"
    )


def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog="For example, run python3 scripts/export_schema.py --project ekyc-iteration --check",
    )
    parser.add_argument(
        "--engine",
        default=os.environ.get("CONTAINER_ENGINE"),
        help="Container CLI (default: CONTAINER_ENGINE, docker, then podman).",
    )
    parser.add_argument(
        "--project",
        default=os.environ.get("COMPOSE_PROJECT_NAME"),
        help="Compose project (default: COMPOSE_PROJECT_NAME or compose.yaml's name).",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=SNAPSHOT,
        help="Schema snapshot path (default: apps/portal/schema/structure.sql).",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Compare the snapshot with the running database and report drift without writing.",
    )
    args = parser.parse_args()
    engine = args.engine or shutil.which("docker") or shutil.which("podman")
    if not engine:
        parser.error("Install Docker Compose or Podman with a Compose provider.")
    schema = export_schema(engine, args.project)
    if args.output.is_file() and args.output.read_text(encoding="utf-8") == schema:
        print(f"Current schema snapshot: {args.output}")
        return
    if args.check:
        raise SystemExit(f"Schema snapshot differs from the running database: {args.output}")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = tempfile.NamedTemporaryFile(dir=args.output.parent, delete=False)
    path = Path(temporary.name)
    try:
        with temporary:
            temporary.write(schema.encode("utf-8"))
        path.chmod(0o644)
        path.replace(args.output)
    finally:
        path.unlink(missing_ok=True)
    print(f"Exported the public application schema to {args.output}")


if __name__ == "__main__":
    main()
