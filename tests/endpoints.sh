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
# Server self-terminates via --timeout after the assertions run, so the only
# cleanup left is the tmpdir. (PIDS kept for the readiness-wait fallback.)
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT" && cargo build --release)
fi

# --timeout caps the server's lifetime so it self-terminates once the test is
# done; we then `wait` on it for its exit status. A short readiness probe
# replaces the old fixed `sleep 1.5`.
HOME="$WORK" "$BIN" \
  --download-dir "$DL" --state-dir "$STATE" \
  --cookies-from-browser none --bind "127.0.0.1:$PORT" \
  --timeout 20 \
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

echo "=== POST /download with empty body ==="
ack=$(curl -s -X POST "$base/download" --data-urlencode 'urls=')
check "empty post" "paste at least one URL" "$ack"

echo "=== POST /download with a failing URL (probe error -> fragment, no queue item) ==="
# A nonexistent YouTube video ID fails at probe time (extraction) deterministically,
# with or without cookies -- yt-dlp prints `ERROR: [youtube] ...: Video unavailable`.
# Under the new flow the probe runs in POST /download itself, so the ERROR is
# returned as an error fragment into #approve and NOTHING is enqueued (a bad URL
# no longer pollutes the queue / state dir).
ack=$(curl -s -X POST "$base/download" --data-urlencode 'urls=https://www.youtube.com/watch?v=aaaaaaaaaaa')
check "probe error fragment" 'class="err"' "$ack"
check "probe error message" "Video unavailable" "$ack"
# Confirm nothing was enqueued: a fresh SSE snapshot has no queue row.
timeout 2 curl -sN "$base/events" > "$WORK/sse.raw" 2>/dev/null || true
if grep -q 'class="row ' "$WORK/sse.raw"; then
  echo "FAIL: a failing-URL probe should not enqueue an item"; fail=1
else
  echo "ok   no item enqueued for failed probe"
fi
# The state dir should still be empty.
if [[ -n "$(ls -A "$STATE" 2>/dev/null)" ]]; then
  echo "FAIL: state dir should be empty after a failed probe"; fail=1
else
  echo "ok   state dir empty after failed probe"
fi

# retry needs a terminal (Failed/Cancelled) queue item. Under the new flow a
# failing URL never queues, so create a Cancelled item by enqueuing a real
# download and cancelling it while it is active, then retry that.
ITEM_ID=""
echo "=== enqueue a real download to cancel + retry ==="
curl -s -X POST "$base/download" --data-urlencode 'urls=https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4' >/dev/null
# Stream SSE briefly, looking for a cancel button on an active row.
timeout 8 curl -sN "$base/events" > "$WORK/sse.retry" 2>/dev/null || true
ACTIVE_ID=$(grep -oE 'cancel/[0-9]+' "$WORK/sse.retry" | head -1 | grep -oE '[0-9]+')
if [[ -n "$ACTIVE_ID" ]]; then
  curl -s -X POST "$base/cancel/$ACTIVE_ID" >/dev/null
  # Give the worker a moment to mark it Cancelled, then look for a retry button.
  sleep 0.5
  timeout 2 curl -sN "$base/events" > "$WORK/sse.retry2" 2>/dev/null || true
  ITEM_ID=$(grep -oE 'retry/[0-9]+' "$WORK/sse.retry2" | head -1 | grep -oE '[0-9]+')
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

# Let the --timeout expire / server exit on its own, surfacing its log.
wait "$SRV" 2>/dev/null || true
echo
echo "=== server log ==="
cat "$WORK/server.log"
echo
exit $fail
