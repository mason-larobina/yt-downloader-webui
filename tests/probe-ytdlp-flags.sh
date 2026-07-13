#!/usr/bin/env bash
# Probe yt-dlp's --newline / --progress-template '%(progress)j' behaviour.
#
# This is the script that resolved DESIGN.md Sec. 11's open question: it
# confirms that the flags emit one newline-terminated JSON object per progress
# tick (no `\r`), that stdout carries progress JSON + [info]/[download]
# Destination: lines, and that stderr carries WARNING:/ERROR:.
#
# Run manually when validating against a new yt-dlp version. Fork it for
# variant probes (e.g. cookies, fragment downloads).
#
# Usage:
#   tests/probe-ytdlp-flags.sh [URL] [DOWNLOAD_DIR]
#
# Defaults to a small archive.org Big Buck Bunny file and a tmpfs dir.
set -euo pipefail

URL="${1:-https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4}"
DIR="${2:-$(mktemp -d -t yt-downloader-webui-probe.XXXXXX)}"
trap 'rm -rf "$DIR"' EXIT

echo "=== yt-dlp version ==="
yt-dlp --version
echo "=== URL: $URL ==="
echo "=== dir: $DIR ==="
echo

# Use -f worst for a small, fast download during probing.
echo "=== full merged output (first 30 lines) ==="
timeout 90 yt-dlp --newline --progress-template '%(progress)j' -P "$DIR" -f worst "$URL" 2>&1 | head -30
echo

echo "=== stdout only (JSON + info lines) ==="
timeout 90 yt-dlp --newline --progress-template '%(progress)j' -P "$DIR" -f worst "$URL" 2>/dev/null | head -6
echo

echo "=== stderr only (WARNING:/ERROR:) ==="
timeout 90 yt-dlp --newline --progress-template '%(progress)j' -P "$DIR" -f worst "$URL" 2>&1 1>/dev/null | head -10
echo

echo "=== error case: unavailable video ==="
echo "-- stdout --"
timeout 30 yt-dlp --newline --progress-template '%(progress)j' -P "$DIR" "https://www.youtube.com/watch?v=BaW_jenozKc" 2>/dev/null
echo "-- stderr --"
timeout 30 yt-dlp --newline --progress-template '%(progress)j' -P "$DIR" "https://www.youtube.com/watch?v=BaW_jenozKc" 1>/dev/null 2>&1
echo

echo "=== files downloaded ==="
ls -la "$DIR"
