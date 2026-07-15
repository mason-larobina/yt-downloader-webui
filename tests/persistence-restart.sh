#!/usr/bin/env bash
# Persistence round-trip test: SIGTERM mid-download -> restart -> re-start.
#
# Verifies DESIGN.md Sec. 8:
#  - On SIGTERM the worker kills the active yt-dlp and flips the active item
#    to Pending (NOT Cancelled), so it is re-started on next launch.
#  - The state file is flushed AFTER the worker finishes its shutdown handling
#    (the item is serialized as "pending", with the right pending count).
#  - On restart the worker auto-starts the pending item and it reaches done.
#
# Run manually after changes to main.rs (shutdown), worker.rs, or persist.rs.
#
# Usage:
#   tests/persistence-restart.sh [URL]
#
# Env:
#   YT_DOWNLOADER_WEBUI_BINARY   path to a prebuilt yt-downloader-webui binary (default: builds one)
#   PORT            listen port (default: 18081)
set -euo pipefail

URL="${1:-https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4}"
PORT="${PORT:-18081}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${YT_DOWNLOADER_WEBUI_BINARY:-$ROOT/target/release/yt-downloader-webui}"

WORK="$(mktemp -d -t yt-downloader-webui-persist.XXXXXX)"
DL="$WORK/dl"; STATE="$WORK/state"; mkdir -p "$DL" "$STATE"
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT" && cargo build --release)
fi

# Wait for the server to accept connections on $PORT (up to ~5s).
wait_for_port() {
  local pid="$1"
  for _ in $(seq 1 50); do
    if curl -s --connect-timeout 1 "http://127.0.0.1:$PORT/library" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then return 1; fi
    sleep 0.1
  done
  return 1
}

# $1 = log file suffix, $2 = --timeout seconds (optional; when given the server
# self-terminates instead of needing an external kill).
start_server() {
  local log="$WORK/server.$1.log"
  local args=(
    --download-dir "$DL" --state-dir "$STATE"
    --cookies-from-browser none --bind "127.0.0.1:$PORT"
  )
  [[ -n "${2:-}" ]] && args+=(--timeout "$2")
  HOME="$WORK" "$BIN" "${args[@]}" > "$log" 2>&1 &
  local pid=$!
  PIDS+=("$pid")
  if ! wait_for_port "$pid"; then
    echo "FATAL: server failed to start"; cat "$log"; exit 1
  fi
  echo "$pid"
}

echo "=== launch #1, queue a download, SIGTERM mid-flight ==="
# Server #1 is deliberately NOT given --timeout: the whole point of this test
# is that SIGTERM mid-flight exercises the graceful-shutdown path.
SRV1=$(start_server 1)
# Drive the new header flow: probe the URL via GET /probe (SSE), then confirm
# the resulting card to actually enqueue it (POST /download only returns the
# probe-area shell; it does not enqueue).
timeout 90 curl -sN --get "http://127.0.0.1:$PORT/probe" --data-urlencode "url=$URL" > "$WORK/probe.raw" 2>/dev/null || true
ENTRY=$(python3 - "$WORK/probe.raw" <<'PY'
import re, sys, html
data = open(sys.argv[1]).read()
idx = data.find("event: result")
if idx < 0:
    sys.exit("no result event")
m = re.search(r'^data: (.*)$', data[idx:], re.M)
if not m:
    sys.exit("no data line")
vals = re.findall(r'name="entry" value="([^"]*)"', m.group(1))
if not vals:
    sys.exit("no entry checkbox")
print(html.unescape(vals[0]))
PY
)
[[ -n "$ENTRY" ]] || { echo "FAIL: probe produced no entry"; cat "$WORK/probe.raw"; exit 1; }
curl -s -X POST "http://127.0.0.1:$PORT/confirm" --data-urlencode "entry=$ENTRY" >/dev/null
echo "queued; waiting 2s for it to go active..."
sleep 2
echo "=== SIGTERM mid-download ==="
kill -TERM "$SRV1"
# wait for the process to actually exit (graceful shutdown)
for _ in $(seq 1 20); do kill -0 "$SRV1" 2>/dev/null || break; sleep 0.2; done

echo "=== server #1 log (expect: 'worker exiting' BEFORE 'flushing queue') ==="
cat "$WORK/server.1.log"

echo "=== state after shutdown (item should be PENDING, not active) ==="
python3 -c "
import json,glob,sys
files=glob.glob('$STATE/*.json')
items=[json.load(open(f)) for f in files]
print('items:', [(i['id'],i['status']) for i in items], 'count:', len(items))
if not items:
    print('FAIL: no state files'); sys.exit(1)
if any(i['status']=='active' for i in items):
    print('FAIL: an item is still active'); sys.exit(1)
"


echo
echo "=== launch #2 (restart): pending item should be re-started automatically ==="
# Server #2 self-terminates via --timeout just past the SSE capture window so
# the run isn't left backgrounded even if the download hangs.
SRV2=$(start_server 2 105)
echo "=== startup log (expect: 'loaded queue: 1 items, 1 pending' + 'will be re-started') ==="
cat "$WORK/server.2.log"

echo "=== opening SSE listener, waiting for re-started item to finish ==="
nohup timeout 90 curl -sN "http://127.0.0.1:$PORT/events" > "$WORK/sse.raw" 2>/dev/null &
SSE=$!
PIDS+=("$SSE")
wait "$SSE" || true
# Let server #2 self-terminate via --timeout so the run isn't left backgrounded.
wait "$SRV2" 2>/dev/null || true

echo "=== final card event (expect 'card done') ==="
# After restart the terminal transition emits a targeted `card-<id>` event
# (not a full-grid `queue` swap), so track the last `card-` event.
awk '/^event: card-/{getline d; last=d} END{print last}' "$WORK/sse.raw" \
  | grep -oE 'card (done|failed)' | head
echo "=== files in download dir ==="
ls -la "$DL"
echo "=== server #2 log (expect 'item N done') ==="
cat "$WORK/server.2.log"

echo
echo "=== assertions ==="
fail=0
# shutdown log order: worker exits before the final flush
if ! grep -q 'worker exiting' "$WORK/server.1.log"; then echo "FAIL: no 'worker exiting' on shutdown"; fail=1; fi
if ! grep -q 'flushing queue on shutdown' "$WORK/server.1.log"; then echo "FAIL: no final flush"; fail=1; fi
# restart picked up the pending item
if ! grep -q 'will be re-started' "$WORK/server.2.log"; then echo "FAIL: pending item not detected on restart"; fail=1; fi
# item reached done after restart
if ! grep -q 'card done' "$WORK/sse.raw" && ! grep -q 'item .* done' "$WORK/server.2.log"; then
  echo "FAIL: re-started item did not reach done"; fail=1
fi
if [[ -z "$(find "$DL" -type f ! -name '.*' -print -quit)" ]]; then echo "FAIL: no file downloaded"; fail=1; fi
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; exit 1; fi
