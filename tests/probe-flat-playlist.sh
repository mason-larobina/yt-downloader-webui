#!/usr/bin/env bash
# Probe yt-dlp's playlist-info extraction (no video download).
#
# This is the validation script for the "expand a playlist URL into per-video
# queue items" feature. It answers the questions the implementation depends on,
# using your real Firefox cookies (so YouTube actually talks to us -- unlike
# the sandbox, which has no Firefox profile and gets a 403 / consent wall).
#
# Run this on your own machine (the one with a logged-in Firefox), then paste
# the full output back so the implementation can be pinned to the real JSON
# shape. Mirrors tests/probe-ytdlp-flags.sh in style.
#
# It answers:
#   1. --flat-playlist -J : top-level + per-entry JSON shape, with cookies.
#   2. --flat-playlist -j : one JSON line per entry (streaming-friendly).
#   3. Which entry field is the *downloadable* per-video URL?  (raw `url`?
#      `webpage_url`? a reconstructed https://www.youtube.com/watch?v=<id>?)
#      We re-feed candidates to `yt-dlp --simulate -J` and compare titles.
#   4. Single (non-playlist) video under --flat-playlist: 1-entry playlist or
#      bare video dict?  (Decides whether the app can treat both uniformly.)
#   5. Timing + payload size (flat extraction only, no download).
#
# Usage:
#   ./tests/probe-flat-playlist.sh [PLAYLIST_URL] [SINGLE_VIDEO_URL] [BROWSER]
#
# Defaults:
#   PLAYLIST_URL    = your YouTube playlist
#   SINGLE_VIDEO_URL = first entry reconstructed from the playlist (auto)
#   BROWSER         = firefox   (leave empty to skip --cookies-from-browser)
#
# Requires: yt-dlp on PATH, python3, jq optional (only for pretty diffs).

set -euo pipefail

PLAYLIST_URL="${1:-https://www.youtube.com/playlist?list=PLEueSxy2K1ZYInz4AIBbSugDZe377cILP}"
SINGLE_VIDEO_URL="${2:-}"     # "" -> auto-derive from first playlist entry
BROWSER="${3:-firefox}"   # "" -> skip --cookies-from-browser

DIR="$(mktemp -d -t yt-downloader-webui-probe-flat.XXXXXX)"
#trap 'rm -rf "$DIR"' EXIT

# Cookie flag, mirroring src/ytdlp.rs / src/config.rs.
COOKIE_FLAG=()
if [[ -n "$BROWSER" ]]; then
    COOKIE_FLAG=(--cookies-from-browser "$BROWSER")
fi

echo "=== yt-dlp version ==="
yt-dlp --version
echo "=== playlist: $PLAYLIST_URL ==="
echo "=== cookies : ${BROWSER} ==="
echo "=== dir    : $DIR ==="
echo

# -----------------------------------------------------------------------------
# 1. --flat-playlist -J  (single JSON object with an `entries` array)
# -----------------------------------------------------------------------------
echo "=== 1. --flat-playlist -J (single blob) ==="
TIMEFORMAT='%3R s'; time yt-dlp \
    --flat-playlist -J --no-warnings --no-progress \
    "${COOKIE_FLAG[@]}" \
    "$PLAYLIST_URL" >"$DIR/flat-J.json" 2>"$DIR/flat-J.err" || true
echo "exit=$?  bytes=$(wc -c <"$DIR/flat-J.json")"
echo "-- stderr (last 5) --"; tail -5 "$DIR/flat-J.err" || true
python3 - "$DIR/flat-J.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print("top-level _type      :", d.get("_type"))
print("top-level extractor  :", d.get("extractor"), "/", d.get("extractor_key"))
print("top-level title/id   :", d.get("title"), "/", d.get("id"))
print("playlist_count       :", d.get("playlist_count"))
entries = d.get("entries") or []
print("n_entries            :", len(entries))
if entries:
    e = entries[0]
    print("-- entry[0] keys (sorted) --")
    print(sorted(e.keys()))
    print("-- entry[0] digest --")
    for k in ("id","title","url","ie_key","extractor","extractor_key",
              "webpage_url","duration","channel","uploader","_type",
              "playlist_index","playlist_id","playlist_title","availability"):
        v = e.get(k)
        if isinstance(v, str) and len(v) > 80: v = v[:80] + "..."
        print(f"  {k:16}: {v!r}")
PY
echo

# -----------------------------------------------------------------------------
# 2. --flat-playlist -j  (one JSON line per entry -- streaming-shaped)
# -----------------------------------------------------------------------------
echo "=== 2. --flat-playlist -j (one line per entry) ==="
TIMEFORMAT='%3R s'; time yt-dlp \
    --flat-playlist -j --no-warnings --no-progress \
    "${COOKIE_FLAG[@]}" \
    "$PLAYLIST_URL" >"$DIR/flat-j.jsonl" 2>"$DIR/flat-j.err" || true
LINES=$(wc -l <"$DIR/flat-j.jsonl" 2>/dev/null || echo 0)
echo "exit=$?  lines=$LINES  bytes=$(wc -c <"$DIR/flat-j.jsonl")"
echo "-- first line keys vs entry[0] from -J (should match) --"
python3 - "$DIR/flat-j.jsonl" <<'PY'
import json, sys
lines = [l for l in open(sys.argv[1]) if l.strip()]
print("non-empty lines:", len(lines))
if lines:
    e = json.loads(lines[0])
    print("line[0] keys:", sorted(e.keys()))
PY
echo

# -----------------------------------------------------------------------------
# 3. Which entry field is the *downloadable* per-video URL?
#    Re-feed candidates to `yt-dlp --simulate -J` (no --flat) and compare title.
# -----------------------------------------------------------------------------
echo "=== 3. per-video URL resolvability (re-fed to --simulate -J) ==="
python3 - "$DIR/flat-J.json" >"$DIR/candidates.txt" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
entries = d.get("entries") or []
if not entries:
    print("# no entries"); sys.exit(0)
e = entries[0]
vid = e.get("id")
cands = []
cands.append(("entry.url", e.get("url")))
cands.append(("entry.webpage_url", e.get("webpage_url")))
if vid:
    cands.append(("reconstructed watch?v=<id>",
                  f"https://www.youtube.com/watch?v={vid}"))
# unique, non-None
seen = set()
for name, val in cands:
    if val and val not in seen:
        seen.add(val)
        print(f"{name}\t{val}")
print(f"# ref title: {e.get('title')!r}  ref id: {vid!r}")
PY
cat "$DIR/candidates.txt"
echo "-- resolving each candidate (title match = good) --"
REF_TITLE=$(sed -n 's/^# ref title: //p' "$DIR/candidates.txt" | tr -d "'")
while IFS=$'\t' read -r name val; do
    [[ "$name" == \#* ]] && continue
    [[ -z "$val" ]] && continue
    # --simulate -J resolves a single video's full metadata without downloading.
    if TITLE=$(yt-dlp --simulate -J --no-warnings --no-progress \
            "${COOKIE_FLAG[@]}" "$val" 2>/dev/null \
            | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('title') or '')" 2>/dev/null); then
        MATCH=$([[ "$TITLE" == "$REF_TITLE" ]] && echo "MATCH" || echo "differs")
        printf '  %-32s -> %s  [%s]\n' "$name" "${TITLE:0:60}" "$MATCH"
    else
        printf '  %-32s -> FAILED to resolve\n' "$name"
    fi
done <"$DIR/candidates.txt"
echo

# -----------------------------------------------------------------------------
# 4. Single (non-playlist) video under --flat-playlist
# -----------------------------------------------------------------------------
if [[ -z "$SINGLE_VIDEO_URL" ]]; then
    # Auto-derive: reconstructed watch URL of the first playlist entry.
    SINGLE_VIDEO_URL=$(python3 - "$DIR/flat-J.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
es = d.get("entries") or []
if es and es[0].get("id"):
    print(f"https://www.youtube.com/watch?v={es[0]['id']}")
PY
)
fi
echo "=== 4. single video under --flat-playlist: $SINGLE_VIDEO_URL ==="
if [[ -n "$SINGLE_VIDEO_URL" ]]; then
    yt-dlp --flat-playlist -J --no-warnings --no-progress \
        "${COOKIE_FLAG[@]}" "$SINGLE_VIDEO_URL" >"$DIR/single-J.json" 2>"$DIR/single-J.err" || true
    echo "exit=$?  bytes=$(wc -c <"$DIR/single-J.json")"
    python3 - "$DIR/single-J.json" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as e:
    print("could not parse:", e); sys.exit(0)
print("  _type             :", d.get("_type"))
print("  extractor         :", d.get("extractor"), "/", d.get("extractor_key"))
print("  has 'entries'     :", "entries" in d, "len:", len(d.get("entries") or []))
print("  looks like video?:", d.get("_type") == "video" or "entries" not in d)
print("  id/title         :", d.get("id"), "/", d.get("title"))
PY
else
    echo "  (skipped: no single-video URL available)"
fi
echo

# -----------------------------------------------------------------------------
# 5. Raw artifacts saved (for pasting back / diffing)
# -----------------------------------------------------------------------------
echo "=== 5. raw artifacts in $DIR (paste these back) ==="
ls -la "$DIR"
echo
echo "Tip: pretty-print with   jq . \"$DIR/flat-J.json\" | head -80"
