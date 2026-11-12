#!/usr/bin/env bash
# Smoke-tests a packaged `gavmo` (the flake's wrapped command): it reports
# the expected version, starts with only the wrapper's defaults, finds qemu,
# logs in and serves the control panel's wasm bundle from its site dir, and
# shuts down cleanly. No VM is booted and no model provider is configured.
#
#   scripts/smoke-test-package.sh <path/to/bin/gavmo> <expected version>
#
# Needs curl. Runs on its own ports (`--ui-port`, `--mcp-port`), so it works
# alongside a running GAVMO daemon.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <path/to/bin/gavmo> <expected version>" >&2
  exit 2
fi
gavmo=$1
version=$2
port=3917
mcp_port=3918

fail() {
  echo "smoke test FAILED: $*" >&2
  if [ -s "$tmp/err" ]; then
    echo "--- daemon stderr ---" >&2
    cat "$tmp/err" >&2
  fi
  exit 1
}

tmp=$(mktemp -d)
pid=
cleanup() {
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT

reported=$("$gavmo" --version)
[ "$reported" = "gavmo $version" ] || fail "--version printed '$reported', expected 'gavmo $version'"

mkdir -p "$tmp/home" "$tmp/data" "$tmp/config" "$tmp/run"
chmod 700 "$tmp/run"
# Unset whatever a developer's shell may export, so only the wrapper's
# defaults are tested (and no model gateway is started).
env -u GAVMO_BASE_IMAGE -u GAVMO_BIFROST -u GAVMO_BIFROST_DATA_DIR \
  -u GAVMO_GCP_PROJECT -u GAVMO_GCP_REGION \
  -u GAVMO_UI_PORT -u GAVMO_MCP_PORT -u GAVMO_BIFROST_PORT \
  -u LEPTOS_OUTPUT_NAME -u LEPTOS_SITE_ROOT -u LEPTOS_SITE_PKG_DIR -u LEPTOS_ENV \
  HOME="$tmp/home" \
  XDG_DATA_HOME="$tmp/data" \
  XDG_CONFIG_HOME="$tmp/config" \
  XDG_RUNTIME_DIR="$tmp/run" \
  GAVMO_GCLOUD_DIR="$tmp/gcloud" \
  "$gavmo" --ui-port "$port" --mcp-port "$mcp_port" > "$tmp/out" 2> "$tmp/err" &
pid=$!

url=
for _ in $(seq 150); do
  url=$(grep -m1 '^http://localhost:' "$tmp/out" || true)
  [ -n "$url" ] && break
  kill -0 "$pid" 2>/dev/null || fail "the daemon exited during startup"
  sleep 0.2
done
[ -n "$url" ] || fail "no login URL within 30 s"
token=${url#*token=}

# The panel only answers to `localhost`; resolve it to the bound address.
curl_panel() {
  curl -sS --resolve "localhost:$port:127.0.0.1" -b "$tmp/jar" -c "$tmp/jar" "$@"
}
curl_panel -o /dev/null "http://localhost:$port/login?token=$token" ||
  fail "login request failed"
grep -q gavmo_session "$tmp/jar" || fail "login set no session cookie"
curl_panel -f -o "$tmp/wasm" "http://localhost:$port/pkg/gavmo.wasm" ||
  fail "could not fetch /pkg/gavmo.wasm (wrong LEPTOS_SITE_ROOT?)"
[ "$(head -c 4 "$tmp/wasm" | od -An -c | tr -d ' ')" = '\0asm' ] ||
  fail "/pkg/gavmo.wasm is not a wasm module"
curl_panel -f -o "$tmp/index" "http://localhost:$port/" || fail "could not fetch /"
grep -q 'gavmo' "$tmp/index" || fail "/ does not look like the control panel"
# The gateway is on --mcp-port and refuses an unauthenticated request.
mcp_status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$mcp_port/mcp") ||
  fail "the MCP gateway is not on port $mcp_port"
[ "$mcp_status" = 401 ] || fail "the MCP gateway answered $mcp_status, expected 401"

kill -TERM "$pid"
status=0
wait "$pid" || status=$?
pid=
[ "$status" -eq 0 ] || fail "the daemon exited with status $status on SIGTERM"

# Startup warns, rather than fails, if qemu isn't on PATH: the wrapper must
# have put it there.
# shellcheck disable=SC2016 # a literal "$PATH" in the daemon's message
if grep -qF 'not found on $PATH' "$tmp/err"; then
  fail "qemu is not on the wrapped daemon's PATH"
fi

echo "smoke test passed: $reported"
