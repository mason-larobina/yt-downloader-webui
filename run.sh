DOWNLOADS_DIR=/tmp/yt-downloader-webui/downloads
STATE_DIR=/tmp/yt-downloader-webui/state
CACHE_DIR=/tmp/yt-downloader-webui/cache

mkdir -p "${DOWNLOADS_DIR}"
mkdir -p "${STATE_DIR}"
mkdir -p "${CACHE_DIR}"

cargo run -- \
  --cookies-from-browser firefox \
  --download-dir "${DOWNLOADS_DIR}" \
  --state-dir "${STATE_DIR}" \
  --cache-dir "${CACHE_DIR}" \
  --bind 0.0.0.0:6789 \
  "${@}"
