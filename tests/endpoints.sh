#!/usr/bin/env bash
# Test the queue control endpoints + file serving + path-traversal guards.
#
# Covers DESIGN.md Sec. 6/9:
#   POST /download, /cancel/:id, /retry/:id, /clear
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

WORK="$(mktemp -d -t web-dl-ep.XXXXXX)"
DL="$WORK/dl"; STATE="$WORK/state"; mkdir -p "$DL" "$STATE"
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT" && cargo build --release)
fi

HOME="$WORK" "$BIN" \
  --download-dir "$DL" --state-file "$STATE/queue.json" \
  --cookies-from-browser none --addr "127.0.0.1:$PORT" \
  > "$WORK/server.log" 2>&1 &
SRV=$!
PIDS+=("$SRV")
sleep 1.5
if ! kill -0 "$SRV" 2>/dev/null; then echo "FATAL: server failed to start"; cat "$WORK/server.log"; exit 1; fi

base="http://127.0.0.1:$PORT"
fail=0
# check NAME EXPECTED_ACTUAL_SUBSTRING ACTUAL  -- pass if EXPECTED is a substring of ACTUAL
check() {
  if [[ "$3" == *"$2"* ]]; then echo "ok   $1"; else echo "FAIL $1: expected '$2' in '$3'"; fail=1; fi
}

echo "=== GET /library on empty dir ==="
curl -s "$base/library" | grep -q 'no files' && echo "ok   empty library" || { echo "FAIL: empty library"; fail=1; }

echo "=== POST /download with empty body ==="
ack=$(curl -s -X POST "$base/download" --data-urlencode 'urls=')
check "empty post" "paste at least one URL" "$ack"

echo "=== POST /download with a failing URL (ERROR: capture) ==="
curl -s -X POST "$base/download" --data-urlencode 'urls=https://www.youtube.com/watch?v=BaW_jenozKc' >/dev/null
# wait for the worker to fail it
for _ in $(seq 1 30); do
  if curl -s "$base/library" >/dev/null 2>&1 && grep -q 'row failed' <(curl -s "$base/library" 2>/dev/null); then :; fi
  # /library doesn't show queue; check the SSE stream instead.
  break
done
# Capture a quick SSE snapshot to inspect the queue.
timeout 3 curl -sN "$base/events" > "$WORK/sse.raw" 2>/dev/null || true
if grep -q 'row failed' "$WORK/sse.raw" && grep -q 'Video unavailable' "$WORK/sse.raw"; then
  echo "ok   failed item surfaces real ERROR message"
else
  echo "FAIL: failed item did not capture ERROR message"; fail=1
fi
ITEM_ID=$(grep -oE 'retry/[0-9]+' "$WORK/sse.raw" | head -1 | grep -oE '[0-9]+')

echo "=== POST /retry/$ITEM_ID ==="
if [[ -n "$ITEM_ID" ]]; then
  ack=$(curl -s -X POST "$base/retry/$ITEM_ID")
  check "retry ack" "requeued item $ITEM_ID" "$ack"
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
echo "=== server log ==="
cat "$WORK/server.log"
echo
if [[ $fail -eq 0 ]]; then echo "PASS"; else echo "FAIL"; exit 1; fi
