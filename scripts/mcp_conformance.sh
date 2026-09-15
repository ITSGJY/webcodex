#!/usr/bin/env bash
set -euo pipefail

# The conformance referee is intentionally immutable. Updating it is a reviewed
# baseline change because scenario/check semantics can change between commits.
BASELINE="tests/fixtures/mcp/conformance/baseline.json"
HARNESS_COMMIT="$(python3 - "$BASELINE" <<'PY'
import json
import pathlib
import re
import sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
commit = value.get("harness_commit", "")
if not re.fullmatch(r"[0-9a-f]{40}", commit):
    raise SystemExit("baseline harness_commit must be a full lowercase Git SHA")
print(commit)
PY
)"
WORK_ROOT="${WEBCODEX_MCP_CONFORMANCE_WORK_ROOT:-target/mcp-conformance}"
HARNESS_DIR="${WEBCODEX_MCP_CONFORMANCE_HARNESS_DIR:-$WORK_ROOT/harness}"
REPORT_ROOT="${WEBCODEX_MCP_CONFORMANCE_REPORT_ROOT:-$WORK_ROOT/reports}"
PROFILES=("$@")

if [ "$#" -eq 0 ]; then
  PROFILES=("2026-07-28" "2025-11-25")
fi
for profile in "${PROFILES[@]}"; do
  case "$profile" in
    2026-07-28|2025-11-25) ;;
    *) echo "unsupported MCP conformance profile: $profile" >&2; exit 2 ;;
  esac
done

mkdir -p "$WORK_ROOT" "$REPORT_ROOT"

prepare_harness() {
  if [ ! -d "$HARNESS_DIR/.git" ]; then
    rm -rf "$HARNESS_DIR"
    mkdir -p "$HARNESS_DIR"
    git -C "$HARNESS_DIR" init -q
    git -C "$HARNESS_DIR" remote add origin https://github.com/modelcontextprotocol/conformance.git
    GIT_TERMINAL_PROMPT=0 git -C "$HARNESS_DIR" fetch -q --depth 1 origin "$HARNESS_COMMIT"
    git -C "$HARNESS_DIR" checkout -q --detach FETCH_HEAD
  fi
  actual="$(git -C "$HARNESS_DIR" rev-parse HEAD)"
  if [ "$actual" != "$HARNESS_COMMIT" ]; then
    echo "MCP conformance harness must be exactly $HARNESS_COMMIT, got $actual" >&2
    exit 2
  fi
  if [ ! -f "$HARNESS_DIR/package-lock.json" ]; then
    echo "pinned harness has no package-lock.json" >&2
    exit 2
  fi
  if [ ! -f "$HARNESS_DIR/dist/index.js" ]; then
    npm --prefix "$HARNESS_DIR" ci --ignore-scripts --no-audit --no-fund
    npm --prefix "$HARNESS_DIR" run build
  fi
}

prepare_harness
python3 scripts/tests/test_mcp_conformance_report.py

# Compile before starting the bounded readiness clock. A cold WebCodex test build
# can take several minutes on small builders; readiness should measure server
# startup, not Rust compilation time.
cargo test --locked -p webcodex --lib mcp_conformance_fixture_server --no-run

fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/webcodex-mcp-conformance.XXXXXX")"
url_file="$fixture_dir/url"
stop_file="$fixture_dir/stop"
fixture_log="$REPORT_ROOT/fixture.log"
fixture_pid=""
cleanup() {
  touch "$stop_file" 2>/dev/null || true
  if [ -n "$fixture_pid" ]; then
    for _ in $(seq 1 50); do
      if ! kill -0 "$fixture_pid" 2>/dev/null; then break; fi
      sleep 0.1
    done
    if kill -0 "$fixture_pid" 2>/dev/null; then
      kill "$fixture_pid" 2>/dev/null || true
    fi
    wait "$fixture_pid" 2>/dev/null || true
  fi
  rm -rf "$fixture_dir"
}
trap cleanup EXIT INT TERM

rm -rf "$REPORT_ROOT"
mkdir -p "$REPORT_ROOT"
WEBCODEX_MCP_CONFORMANCE_URL_FILE="$url_file" \
WEBCODEX_MCP_CONFORMANCE_STOP_FILE="$stop_file" \
  cargo test --locked -p webcodex --lib mcp_conformance_fixture_server -- --ignored --nocapture \
  >"$fixture_log" 2>&1 &
fixture_pid=$!

ready_deadline=$((SECONDS + 60))
while [ ! -s "$url_file" ]; do
  if ! kill -0 "$fixture_pid" 2>/dev/null; then
    echo "MCP conformance fixture exited before publishing its URL" >&2
    tail -80 "$fixture_log" >&2 || true
    exit 1
  fi
  if [ "$SECONDS" -ge "$ready_deadline" ]; then
    echo "timed out waiting for MCP conformance fixture URL" >&2
    tail -80 "$fixture_log" >&2 || true
    exit 1
  fi
  sleep 0.1
done
fixture_url="$(tr -d '\r\n' < "$url_file")"
case "$fixture_url" in
  http://127.0.0.1:*'/mcp') ;;
  *) echo "fixture published unexpected URL: $fixture_url" >&2; exit 1 ;;
esac

server_sha="$(git rev-parse HEAD)"
overall=0
for profile in "${PROFILES[@]}"; do
  profile_root="$REPORT_ROOT/$profile"
  raw="$profile_root/raw"
  mkdir -p "$raw"
  log="$profile_root/harness.log"
  set +e
  node "$HARNESS_DIR/dist/index.js" server \
    --url "$fixture_url" \
    --requirements "$profile" \
    --output-dir "$raw" \
    --timeout 10000 \
    >"$log" 2>&1
  harness_exit=$?
  set -e

  metadata="$profile_root/metadata.json"
  python3 - "$HARNESS_DIR/requirements/$profile.yaml" "$metadata" "$profile" \
    "$server_sha" "$HARNESS_COMMIT" "$harness_exit" <<'PY'
import json
import pathlib
import sys

requirements_path, output_path, profile, server_sha, harness_sha, harness_exit = sys.argv[1:]
server = []
in_server = False
for raw in pathlib.Path(requirements_path).read_text(encoding="utf-8").splitlines():
    if raw == "server:":
        in_server = True
        continue
    if in_server and raw and not raw.startswith(" "):
        break
    if in_server and raw.startswith("  - "):
        server.append(raw[4:].strip())
if not server:
    raise SystemExit(f"no scored server scenarios found in {requirements_path}")
pathlib.Path(output_path).write_text(
    json.dumps(
        {
            "schema_version": 1,
            "profile": profile,
            "server_sha": server_sha,
            "harness_sha": harness_sha,
            "harness_exit_code": int(harness_exit),
            "required_scenarios": server,
        },
        indent=2,
        sort_keys=True,
    ) + "\n",
    encoding="utf-8",
)
PY

  summary="$profile_root/summary.json"
  if ! python3 scripts/mcp_conformance_report.py \
      --reports "$raw" \
      --metadata "$metadata" \
      --baseline "$BASELINE" \
      --profile "$profile" \
      --summary "$summary"; then
    overall=1
  fi
done

exit "$overall"
