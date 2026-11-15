#!/usr/bin/env bash
# Disposable local application instances against an already-running local Restate + PostgreSQL.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${DATABASE_URL:?Set DATABASE_URL to a disposable local PostgreSQL database}"
export RESTATE_INGRESS_URL="${RESTATE_INGRESS_URL:-http://127.0.0.1:8080}"
admin="${RESTATE_ADMIN_URL:-http://127.0.0.1:9070}"
bin_dir="${EKYC_BIN_DIR:-${CARGO_TARGET_DIR:-target}/debug}"
for executable in ekyc-worker ekyc-api ekyc-webhook; do
  test -x "$bin_dir/$executable" || { echo "Build the workspace first (missing $executable)" >&2; exit 1; }
done
run_dir=$(mktemp -d "${TMPDIR:-/tmp}/ekyc-smoke.XXXXXXXX")
children=()
cleanup() {
  for pid in "${children[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${children[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
start() {
  local name="$1"; shift
  "$@" >"$run_dir/$name.log" 2>&1 &
  last_child=$!
  children+=("$last_child")
}
export EKYC_DEMO_CALLBACKS=1
export DOCUMENT_URL=http://127.0.0.1:9801 FACE_URL=http://127.0.0.1:9802
export ADDRESS_URL=http://127.0.0.1:9803 SANCTIONS_URL=http://127.0.0.1:9804
export WORKER_BIND=127.0.0.1:9080 API_BIND=127.0.0.1:8090
export WEBHOOK_TOKEN=local-demo-events-token WEBHOOK_URL=http://127.0.0.1:8110/events
export WEBHOOK_DB="$run_dir/events.sqlite3" PORT=8110
export EKYC_EVIDENCE_URL=http://127.0.0.1:9805 EKYC_ISSUER_URL=http://127.0.0.1:9805
export EKYC_EVIDENCE_TOKEN=local-demo-evidence-issuer
export EKYC_CLIENTS_FILE="$PWD/config/demo-clients.json"
echo "Application logs: $run_dir"
port=9801
for provider in document face address sanctions; do
  start "$provider" env INTERACTIVE_DB_PATH="$run_dir/$provider.sqlite3" python3 mocks/server.py "$provider" --host 127.0.0.1 --port "$port"
  port=$((port + 1))
done
start issuer env PORT=9805 ISSUER_DB="$run_dir/issuer.sqlite3" python3 mocks/evidence.py
start sink python3 mocks/webhook_sink.py
start worker "$bin_dir/ekyc-worker"
worker_pid=$last_child
start api "$bin_dir/ekyc-api"
start webhook "$bin_dir/ekyc-webhook"
python3 scripts/register.py "$admin" http://127.0.0.1:9080
python3 scripts/product_smoke.py
python3 scripts/interactive_smoke.py
python3 scripts/binding_drift_smoke.py
python3 scripts/queries_smoke.py
if [[ "${EKYC_RECONCILIATION_TEST:-0}" == "1" ]]; then
  python3 scripts/reconciliation_smoke.py
  python3 scripts/provider_ordering_smoke.py
fi
if [[ "${EKYC_LATE_SCREEN_TEST:-0}" == "1" ]]; then
  python3 scripts/late_screen_smoke.py
fi
if [[ "${EKYC_RECOVERY_TEST:-0}" == "1" ]]; then
  python3 scripts/recovery_smoke.py prepare "$run_dir/product-recovery.json"
  kill -KILL "$worker_pid"
  wait "$worker_pid" 2>/dev/null || true
  start worker-replacement "$bin_dir/ekyc-worker"
  worker_pid=$last_child
  python3 scripts/recovery_smoke.py verify "$run_dir/product-recovery.json"
fi
