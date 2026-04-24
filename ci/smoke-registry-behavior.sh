#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-target/debug/pocker}"
WORKDIR="$(mktemp -d)"
SERVER_LOG="${WORKDIR}/server.log"
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]]; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" >/dev/null 2>&1 || true
  fi
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT

command -v python3 >/dev/null 2>&1 || {
  echo "missing required tool: python3" >&2
  exit 1
}

PORT="$(
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"

python3 -u - "${PORT}" >"${SERVER_LOG}" 2>&1 <<'PY' &
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port = int(sys.argv[1])

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(503)
        self.send_header("Retry-After", "0")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, fmt, *args):
        pass

ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
SERVER_PID=$!

sleep 1

set +e
OUTPUT="$(timeout 90s "${BIN}" --cache-dir "${WORKDIR}/cache" pull --plain-http "127.0.0.1:${PORT}/sample:latest" 2>&1)"
STATUS=$?
set -e

if [[ "${STATUS}" -eq 0 ]]; then
  echo "expected pull against retry server to fail" >&2
  echo "${OUTPUT}" >&2
  exit 1
fi

grep -q "retry limit exceeded for registry request" <<<"${OUTPUT}" || {
  echo "missing retry exhaustion message in CLI output" >&2
  echo "${OUTPUT}" >&2
  exit 1
}

echo "smoke: registry retry behavior checks passed"
