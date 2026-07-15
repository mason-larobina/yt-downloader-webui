#!/usr/bin/env bash
# End-to-end smoke test for the yt-downloader-webui server.
#
# Builds the release binary, starts the server with --cookies-from-browser none
# (no Firefox in CI/sandbox), then drives the new header flow end-to-end:
#   1. POST /download -> probe-area shell (sse-connect wired up)
#   2. GET  /probe?url=... (SSE) -> streams log lines, emits a `result` event
#      carrying one confirm card (single video)
#   3. POST /confirm with that card's entry JSON -> enqueues, header restored
#   4. the global /events SSE stream shows progress/queue/log/library, the
#      file lands in the download dir, and the queue item reaches done.
#
# Run manually after changes to worker.rs / parse.rs / render.rs / server.rs.
# Fork it to test specific sites or cookie stores.
#
# Usage:
#   tests/e2e-download.sh [URL]
#
# Env:
#   YT_DOWNLOADER_WEBUI_BINARY   path to a prebuilt yt-downloader-webui binary (default: builds one)
#   PORT            listen port (default: 18080)
#   TIMEOUT         SSE capture window in seconds (default: 90)
set -euo pipefail

URL="${1:-https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4}"
PORT="${PORT:-18080}"
TIMEOUT="${TIMEOUT:-90}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${YT_DOWNLOADER_WEBUI_BINARY:-$ROOT/target/release/yt-downloader-webui}"

WORK="$(mktemp -d -t yt-downloader-webui-e2e.XXXXXX)"
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
  --timeout $((TIMEOUT + 30)) \
  > "$WORK/server.log" 2>&1 &
SRV=$!
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

base="http://127.0.0.1:$PORT"

# Drive the probe -> confirm flow, returning the entry JSON that was confirmed
# (printed by the python helper). Captures the /probe SSE stream to $1.
probe_and_confirm() {
  local out="$1"
  echo "=== GET /probe (SSE) for $URL ==="
  timeout 90 curl -sN --get "$base/probe" --data-urlencode "url=$URL" > "$out" 2>/dev/null || true

  # Extract the first entry checkbox value (HTML-unescaped JSON) from the
  # `result` event payload, then POST it to /confirm.
  local entry
  entry=$(python3 - "$out" <<'PY'
import re, sys, html
data = open(sys.argv[1]).read()
idx = data.find("event: result")
if idx < 0:
    sys.exit("no result event in /probe stream")
chunk = data[idx:]
m = re.search(r'^data: (.*)$', chunk, re.M)
if not m:
    sys.exit("result event has no data line")
payload = m.group(1)
vals = re.findall(r'name="entry" value="([^"]*)"', payload)
if not vals:
    sys.exit("result event has no entry checkbox")
print(html.unescape(vals[0]))
PY
  )
  if [[ -z "$entry" ]]; then
    echo "FAIL: probe produced no confirmable entry"; cat "$out"; exit 1
  fi
  echo "=== POST /confirm (entry=$entry) ==="
  curl -s -X POST "$base/confirm" --data-urlencode "entry=$entry"
  echo " <- header restored"
}

echo "=== opening global SSE listener (timeout ${TIMEOUT}s) ==="
timeout "$TIMEOUT" curl -sN "$base/events" > "$WORK/sse.raw" 2>/dev/null &
SSE=$!

# Give the listener a moment to connect before we enqueue.
sleep 0.5
probe_and_confirm "$WORK/probe.raw"

wait "$SSE" || true

# Wait for the server to self-terminate (its --timeout has expired by now) so
# the run isn't left backgrounded and the queue flush lands before we read it.
wait "$SRV" 2>/dev/null || true

echo
echo "=== /probe event counts ==="
grep -o '^event: [a-z]*' "$WORK/probe.raw" | sort | uniq -c
echo
echo "=== /events event counts ==="
grep -o '^event: [a-z]*' "$WORK/sse.raw" | sort | uniq -c
echo
echo "=== final card event (expect 'card done') ==="
# The terminal transition emits a targeted `card-<id>` event (not a full-grid
# `queue` swap), so track the last `card-` event -- its payload is the card
# HTML, whose root class carries the new status.
awk '/^event: card-/{getline d; last=d} END{print last}' "$WORK/sse.raw" \
  | grep -oE 'card (done|failed)' | head
echo
echo "=== last cards-count event (expect '1 total, 0 pending') ==="
awk '/^event: cards-count$/{getline d; last=d} END{print last}' "$WORK/sse.raw"
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
if ! grep -q '^event: log' "$WORK/probe.raw"; then
  echo "FAIL: /probe streamed no log lines"; fail=1
fi
if ! grep -q '^event: result' "$WORK/probe.raw"; then
  echo "FAIL: /probe emitted no result event"; fail=1
fi
if ! grep -q 'card done' "$WORK/sse.raw"; then
  echo "FAIL: no 'card done' in /events stream"; fail=1
fi
if ! grep -q '^event: cards-count$' "$WORK/sse.raw"; then
  echo "FAIL: no targeted cards-count event in /events stream"; fail=1
fi
if ! grep -q '^event: card-' "$WORK/sse.raw"; then
  echo "FAIL: no targeted card-<id> event in /events stream"; fail=1
fi
if ! awk '/^event: status$/{getline d; last=d} END{print last}' "$WORK/sse.raw" | grep -q 'banner idle'; then
  echo "FAIL: final status is not idle"; fail=1
fi
if [[ -z "$(find "$DL" -type f ! -name '.*' -print -quit)" ]]; then
  echo "FAIL: no file downloaded"; fail=1
fi
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; exit 1; fi
