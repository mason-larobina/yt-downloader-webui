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
# --timeout caps the server's lifetime just past the SSE capture window so it
# self-terminates even if the test hangs; we `wait` on it for its exit status.
HOME="$WORK" "$BIN" \
  --download-dir "$DL" --state-dir "$STATE" \
  --cookies-from-browser none --bind "127.0.0.1:$PORT" \
  --timeout $((TIMEOUT + 15)) \
  > "$WORK/server.log" 2>&1 &
SRV=$!
# No kill trap: the server self-terminates via --timeout. tmpdir still cleaned.
wait_for_port() {
  for _ in $(seq 1 50); do
    if curl -s --connect-timeout 1 "http://127.0.0.1:$PORT/library" >/dev/null 2>&1; then return 0; fi
    if ! kill -0 "$SRV" 2>/dev/null; then return 1; fi
    sleep 0.1
  done
  return 1
}
if ! wait_for_port; then
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

# Wait for the server to self-terminate (its --timeout has expired by now) so
# the run isn't left backgrounded and the queue flush lands before we read it.
wait "$SRV" 2>/dev/null || true

echo
echo "=== event counts ==="
grep -o '^event: [a-z]*' "$WORK/sse.raw" | sort | uniq -c
echo
echo "=== final queue event (expect 'card done') ==="
awk '/^event: queue$/{getline d; last=d} END{print last}' "$WORK/sse.raw" \
  | grep -oE 'card (done|failed)|cards-count">[0-9]+ total' | head
echo
echo "=== last status event (expect idle 'queue empty', NOT a 0% bar) ==="
awk '/^event: status$/{getline d; last=d} END{print last}' "$WORK/sse.raw"
echo
echo "=== files in download dir ==="
ls -la "$DL"
echo
echo "=== state dir (one <ts>.json per item) ==="
ls -la "$STATE"
for f in "$STATE"/*.json; do echo "--- $f ---"; cat "$f"; done
echo
echo "=== server log ==="
cat "$WORK/server.log"

# Assertions
echo
echo "=== assertions ==="
fail=0
if ! grep -q 'card done' "$WORK/sse.raw"; then
  echo "FAIL: no 'card done' in SSE stream"; fail=1
fi
if ! awk '/^event: status$/{getline d; last=d} END{print last}' "$WORK/sse.raw" | grep -q 'status idle'; then
  echo "FAIL: final status is not idle"; fail=1
fi
if [[ -z "$(find "$DL" -type f ! -name '.*' -print -quit)" ]]; then
  echo "FAIL: no file downloaded"; fail=1
fi
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; exit 1; fi
