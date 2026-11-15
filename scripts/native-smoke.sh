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
export DOCUMENT_URL=http://127.0.0.1:9801 FACE_URL=http://127.0.0.1:9802
export ADDRESS_URL=http://127.0.0.1:9803 SANCTIONS_URL=http://127.0.0.1:9804
export WORKER_BIND=127.0.0.1:9080 API_BIND=127.0.0.1:8090
export WEBHOOK_TOKEN=local-demo-events-token WEBHOOK_URL=http://127.0.0.1:8110/events
export WEBHOOK_DB="$run_dir/events.sqlite3" PORT=8110
export EKYC_CLIENTS='[{"id":"demo","token":"local-demo-onboarding","purposes":["full_onboarding","address_refresh"]},{"id":"partner","token":"local-demo-refresher","purposes":["address_refresh"]},{"id":"quota-test","token":"local-demo-quota-test","purposes":["address_refresh"]}]'
echo "Application logs: $run_dir"
port=9801
for provider in document face address sanctions; do
  start "$provider" python3 mocks/server.py "$provider" --host 127.0.0.1 --port "$port"
  port=$((port + 1))
done
start sink python3 mocks/webhook_sink.py
start worker "$bin_dir/ekyc-worker"
worker_pid=$last_child
start api "$bin_dir/ekyc-api"
start webhook "$bin_dir/ekyc-webhook"
python3 scripts/register.py "$admin" http://127.0.0.1:9080
python3 scripts/smoke.py
if [[ "${EKYC_RECOVERY_TEST:-0}" == "1" ]]; then
  WORKER_PID="$worker_pid" EKYC_WORKER_BIN="$bin_dir/ekyc-worker" python3 scripts/recovery_smoke.py --crash-disposable-worker
fi
