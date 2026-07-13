#!/usr/bin/env bash
# Test the queue control endpoints + file serving + path-traversal guards.
#
# Covers DESIGN.md Sec. 6/9:
#   POST /download (header -> probe-area shell), GET /probe (SSE result),
#   POST /confirm (enqueue selected), /cancel/:id, /retry/:id, /clear
#   GET  /library, /file/:name (inline + download, range)
#   POST /delete/:name
#   Path traversal: /file/.., /file/<encoded> -> 404
#   ERROR: capture into item.error (failed item shows real yt-dlp message)
#
# Run manually after changes to server.rs / library.rs / worker.rs.
# Fork to add more edge cases.
#
# Usage:
#   tests/endpoints.sh
#
# Env:
#   WEB_DL_BINARY   path to a prebuilt web-dl binary (default: builds one)
#   PORT            listen port (default: 18082)
set -euo pipefail

PORT="${PORT:-18082}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${WEB_DL_BINARY:-$ROOT/target/release/web-dl}"

# Wait for the server to accept connections on $1 (up to ~5s).
wait_for_port() {
  local port="$1"
  for _ in $(seq 1 50); do
    if curl -s --connect-timeout 1 "http://127.0.0.1:$port/library" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$SRV" 2>/dev/null; then return 1; fi
    sleep 0.1
  done
  return 1
}

WORK="$(mktemp -d -t web-dl-ep.XXXXXX)"
DL="$WORK/dl"; STATE="$WORK/state"; mkdir -p "$DL" "$STATE"
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT" && cargo build --release)
fi

HOME="$WORK" "$BIN" \
  --download-dir "$DL" --state-dir "$STATE" \
  --cookies-from-browser none --bind "127.0.0.1:$PORT" \
  --timeout 30 \
  > "$WORK/server.log" 2>&1 &
SRV=$!
PIDS+=("$SRV")
if ! wait_for_port "$PORT"; then
  echo "FATAL: server failed to start"; cat "$WORK/server.log"; exit 1
fi

base="http://127.0.0.1:$PORT"
fail=0
# check NAME EXPECTED_ACTUAL_SUBSTRING ACTUAL  -- pass if EXPECTED is a substring of ACTUAL
check() {
  if [[ "$3" == *"$2"* ]]; then echo "ok   $1"; else echo "FAIL $1: expected '$2' in '$3'"; fail=1; fi
}

echo "=== GET /library on empty dir ==="
curl -s "$base/library" | grep -q 'no files' && echo "ok   empty library" || { echo "FAIL: empty library"; fail=1; }

echo "=== GET /header returns the input form ==="
frag=$(curl -s "$base/header")
check "header form posts /download" 'hx-post="/download"' "$frag"
check "header form has url field" 'name="url"' "$frag"

echo "=== POST /download with empty body -> inline error form ==="
ack=$(curl -s -X POST "$base/download" --data-urlencode 'url=')
check "empty post" "paste a URL" "$ack"
check "empty post restores form" 'name="url"' "$ack"

echo "=== POST /download with a URL -> probe-area shell (sse-connect wired) ==="
frag=$(curl -s -X POST "$base/download" --data-urlencode "url=https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4")
check "probe-area shell" 'id="probe-area"' "$frag"
check "probe-area sse-connect" 'sse-connect="/probe?url=' "$frag"
check "probe-area has cancel" 'hx-get="/header"' "$frag"

echo "=== GET /probe with a failing URL (SSE result -> error, no enqueue) ==="
# A nonexistent YouTube video ID fails at probe time (extraction) deterministically,
# with or without cookies -- yt-dlp prints `ERROR: [youtube] ...: Video unavailable`.
# Under the new flow the probe streams on GET /probe, so the ERROR arrives in the
# `result` event (an error + Done button), and NOTHING is enqueued.
timeout 30 curl -sN --get "$base/probe" --data-urlencode 'url=https://www.youtube.com/watch?v=aaaaaaaaaaa' > "$WORK/probe.err" 2>/dev/null || true
result_data=$(awk 'f&&/^data: /{sub(/^data: /,""); print; exit} /^event: result$/{f=1}' "$WORK/probe.err")
check "probe error fragment" 'class="err"' "$result_data"
check "probe error message" "Video unavailable" "$result_data"
check "probe error has Done button" '>Done<' "$result_data"
# Confirm nothing was enqueued: a fresh SSE snapshot has no queue row.
timeout 2 curl -sN "$base/events" > "$WORK/sse.raw" 2>/dev/null || true
if grep -q 'class="card ' "$WORK/sse.raw"; then
  echo "FAIL: a failing-URL probe should not enqueue an item"; fail=1
else
  echo "ok   no item enqueued for failed probe"
fi
if [[ -n "$(ls -A "$STATE" 2>/dev/null)" ]]; then
  echo "FAIL: state dir should be empty after a failed probe"; fail=1
else
  echo "ok   state dir empty after failed probe"
fi

# Drive the probe -> confirm flow for a real URL, returning the entry JSON.
probe_and_confirm() {
  local url="$1"
  timeout 90 curl -sN --get "$base/probe" --data-urlencode "url=$url" > "$WORK/probe.ok" 2>/dev/null || true
  python3 - "$WORK/probe.ok" <<'PY'
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
}

# retry needs a terminal (Failed/Cancelled) queue item. Create a Cancelled item
# by confirming a real download and cancelling it while it is active, then retry.
echo "=== probe + confirm a real download to cancel + retry ==="
ENTRY=$(probe_and_confirm "https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4")
curl -s -X POST "$base/confirm" --data-urlencode "entry=$ENTRY" >/dev/null
# Stream SSE briefly, looking for a cancel button on an active row.
timeout 8 curl -sN "$base/events" > "$WORK/sse.retry" 2>/dev/null || true
ACTIVE_ID=$(grep -oE 'cancel/[0-9]+' "$WORK/sse.retry" | head -1 | grep -oE '[0-9]+')
if [[ -n "$ACTIVE_ID" ]]; then
  curl -s -X POST "$base/cancel/$ACTIVE_ID" >/dev/null
  sleep 0.5
  timeout 2 curl -sN "$base/events" > "$WORK/sse.retry2" 2>/dev/null || true
  ITEM_ID=$(grep -oE 'retry/[0-9]+' "$WORK/sse.retry2" | head -1 | grep -oE '[0-9]+')
else
  ITEM_ID=""
fi

echo "=== POST /retry/${ITEM_ID:-<none>} ==="
if [[ -n "$ITEM_ID" ]]; then
  ack=$(curl -s -X POST "$base/retry/$ITEM_ID")
  check "retry ack" "requeued item $ITEM_ID" "$ack"
else
  echo "SKIP retry: the download finished before we could cancel it (try a slower URL)"
fi

echo "=== POST /clear ==="
ack=$(curl -s -X POST "$base/clear")
echo "clear ack: $ack"

echo "=== create a fake file and test /file, /delete, traversal ==="
echo "fake video data" > "$DL/sample.mp4"

echo "--- GET /library (should list sample.mp4) ---"
curl -s "$base/library" | grep -q 'sample.mp4' && echo "ok   library lists file" || { echo "FAIL: library"; fail=1; }

echo "--- GET /file/sample.mp4?download=1 (attachment, content-type) ---"
curl -s -D - -o /dev/null "$base/file/sample.mp4?download=1" | grep -i 'content-disposition: attachment' >/dev/null \
  && echo "ok   attachment" || { echo "FAIL: attachment"; fail=1; }
curl -s -D - -o /dev/null "$base/file/sample.mp4?download=1" | grep -i 'content-type: video/mp4' >/dev/null \
  && echo "ok   content-type" || { echo "FAIL: content-type"; fail=1; }

echo "--- GET /file/sample.mp4?inline=1 ---"
curl -s -D - -o /dev/null "$base/file/sample.mp4?inline=1" | grep -i 'content-disposition: inline' >/dev/null \
  && echo "ok   inline" || { echo "FAIL: inline"; fail=1; }

echo "--- range request ---"
curl -s -D - -o /dev/null -H 'Range: bytes=0-3' "$base/file/sample.mp4?inline=1" | grep -i 'content-range: bytes 0-3' >/dev/null \
  && echo "ok   range" || { echo "FAIL: range"; fail=1; }

echo "--- path traversal guards ---"
code=$(curl -s --path-as-is -o /dev/null -w '%{http_code}' "$base/file/..?download=1")
check "traversal .." "404" "$code"
code=$(curl -s --path-as-is -o /dev/null -w '%{http_code}' "$base/file/%2e%2e%2fpasswd?download=1")
check "traversal encoded" "404" "$code"
code=$(curl -s --path-as-is -o /dev/null -w '%{http_code}' "$base/file/.hidden?download=1")
check "dotfile" "404" "$code"

echo "--- POST /delete/sample.mp4 ---"
ack=$(curl -s -X POST "$base/delete/sample.mp4")
echo "delete ack: $ack"
[[ -e "$DL/sample.mp4" ]] && { echo "FAIL: file still exists after delete"; fail=1; } || echo "ok   deleted"

echo
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; fi

wait "$SRV" 2>/dev/null || true
echo
echo "=== server log ==="
cat "$WORK/server.log"
echo
exit $fail
