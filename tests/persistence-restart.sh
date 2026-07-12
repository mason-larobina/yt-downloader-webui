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
#   WEB_DL_BINARY   path to a prebuilt web-dl binary (default: builds one)
#   PORT            listen port (default: 18081)
set -euo pipefail

URL="${1:-https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4}"
PORT="${PORT:-18081}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${WEB_DL_BINARY:-$ROOT/target/release/web-dl}"

WORK="$(mktemp -d -t web-dl-persist.XXXXXX)"
DL="$WORK/dl"; STATE="$WORK/state"; mkdir -p "$DL" "$STATE"
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT" && cargo build --release)
fi

start_server() {  # $1 = log file suffix
  local log="$WORK/server.$1.log"
  HOME="$WORK" "$BIN" \
    --download-dir "$DL" --state-file "$STATE/queue.json" \
    --cookies-from-browser none --addr "127.0.0.1:$PORT" \
    > "$log" 2>&1 &
  local pid=$!
  PIDS+=("$pid")
  sleep 1.5
  if ! kill -0 "$pid" 2>/dev/null; then echo "FATAL: server failed to start"; cat "$log"; exit 1; fi
  echo "$pid"
}

echo "=== launch #1, queue a download, SIGTERM mid-flight ==="
SRV1=$(start_server 1)
curl -s -X POST "http://127.0.0.1:$PORT/download" --data-urlencode "urls=$URL" >/dev/null
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
import json,sys
d=json.load(open('$STATE/queue.json'))
print('items:', [(i['id'],i['status']) for i in d['items']], 'next_id:', d['next_id'])
ok = all(i['status']=='pending' for i in d['items'] if i['status']=='active') or \
     all(i['status']!='active' for i in d['items'])
sys.exit(0 if all(i['status']!='active' for i in d['items']) else 1)
" || { echo "FAIL: an item is still 'active' in the state file"; exit 1; }

echo
echo "=== launch #2 (restart): pending item should be re-started automatically ==="
SRV2=$(start_server 2)
echo "=== startup log (expect: 'loaded queue: 1 items, 1 pending' + 'will be re-started') ==="
cat "$WORK/server.2.log"

echo "=== opening SSE listener, waiting for re-started item to finish ==="
nohup timeout 90 curl -sN "http://127.0.0.1:$PORT/events" > "$WORK/sse.raw" 2>/dev/null &
SSE=$!
PIDS+=("$SSE")
wait "$SSE" || true

echo "=== final queue event (expect 'row done') ==="
awk '/^event: queue$/{getline d; last=d} END{print last}' "$WORK/sse.raw" \
  | grep -oE 'row (done|failed)' | head
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
if ! grep -q 'row done' "$WORK/sse.raw" && ! grep -q 'item .* done' "$WORK/server.2.log"; then
  echo "FAIL: re-started item did not reach done"; fail=1
fi
if [[ -z "$(find "$DL" -type f ! -name '.*' -print -quit)" ]]; then echo "FAIL: no file downloaded"; fail=1; fi
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; exit 1; fi
