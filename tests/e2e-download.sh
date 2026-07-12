#!/usr/bin/env bash
# End-to-end smoke test for the web-dl server.
#
# Builds the release binary, starts the server with --cookies-from-browser none
# (no Firefox in CI/sandbox), opens an SSE listener, POSTs a URL, and verifies
# the worker drains the queue: progress/status/log/library SSE fragments
# render, the file lands in the download dir, and the queue item reaches done.
#
# Run manually after changes to worker.rs / parse.rs / render.rs / server.rs.
# Fork it to test specific sites or cookie stores.
#
# Usage:
#   tests/e2e-download.sh [URL]
#
# Env:
#   WEB_DL_BINARY   path to a prebuilt web-dl binary (default: builds one)
#   PORT            listen port (default: 18080)
#   TIMEOUT         SSE capture window in seconds (default: 90)
set -euo pipefail

URL="${1:-https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4}"
PORT="${PORT:-18080}"
TIMEOUT="${TIMEOUT:-90}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${WEB_DL_BINARY:-$ROOT/target/release/web-dl}"

WORK="$(mktemp -d -t web-dl-e2e.XXXXXX)"
# shellcheck disable=SC2064
trap "rm -rf '$WORK'" EXIT
DL="$WORK/dl"; STATE="$WORK/state"; mkdir -p "$DL" "$STATE"

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT" && cargo build --release)
fi

echo "=== starting server on 127.0.0.1:$PORT ==="
HOME="$WORK" "$BIN" \
  --download-dir "$DL" --state-file "$STATE/queue.json" \
  --cookies-from-browser none --bind "127.0.0.1:$PORT" \
  > "$WORK/server.log" 2>&1 &
SRV=$!
# shellcheck disable=SC2064
trap "kill '$SRV' 2>/dev/null || true; rm -rf '$WORK'" EXIT
sleep 1.5
if ! kill -0 "$SRV" 2>/dev/null; then
  echo "FATAL: server failed to start"; cat "$WORK/server.log"; exit 1
fi
echo "server pid $SRV"
cat "$WORK/server.log"

echo "=== opening SSE listener (timeout ${TIMEOUT}s) ==="
timeout "$TIMEOUT" curl -sN "http://127.0.0.1:$PORT/events" > "$WORK/sse.raw" 2>/dev/null &
SSE=$!

echo "=== POST /download ==="
sleep 0.5
curl -s -X POST "http://127.0.0.1:$PORT/download" --data-urlencode "urls=$URL"
echo " <- ack"

wait "$SSE" || true

echo
echo "=== event counts ==="
grep -o '^event: [a-z]*' "$WORK/sse.raw" | sort | uniq -c
echo
echo "=== final queue event (expect 'row done') ==="
awk '/^event: queue$/{getline d; last=d} END{print last}' "$WORK/sse.raw" \
  | grep -oE 'row (done|failed)|queue \([0-9]+, [0-9]+ pending\)' | head
echo
echo "=== last status event (expect idle 'queue empty', NOT a 0% bar) ==="
awk '/^event: status$/{getline d; last=d} END{print last}' "$WORK/sse.raw"
echo
echo "=== files in download dir ==="
ls -la "$DL"
echo
echo "=== state file ==="
cat "$STATE/queue.json"
echo
echo "=== server log ==="
cat "$WORK/server.log"

# Assertions
echo
echo "=== assertions ==="
fail=0
if ! grep -q 'row done' "$WORK/sse.raw"; then
  echo "FAIL: no 'row done' in SSE stream"; fail=1
fi
if ! awk '/^event: status$/{getline d; last=d} END{print last}' "$WORK/sse.raw" | grep -q 'status idle'; then
  echo "FAIL: final status is not idle"; fail=1
fi
if [[ -z "$(find "$DL" -type f ! -name '.*' -print -quit)" ]]; then
  echo "FAIL: no file downloaded"; fail=1
fi
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; exit 1; fi
