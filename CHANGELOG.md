# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-07-16

First packaged release of `yt-downloader-webui`: a standalone, single-binary web
UI wrapper around `yt-dlp`. This version supersedes the unpublished `0.1.0`
baseline that accumulated during initial development.

### Added
- Standalone single-binary web UI wrapping `yt-dlp`: paste URLs, queue
  downloads, watch live progress in the browser.
- Streaming `/probe` view with thumbnail fetching and `ffmpeg` fallback for
  native frame extraction (content-addressed, log₂-anchored frame counts).
- Per-video details page with file size, real format, and a thumbnail grid.
- Targeted Server-Sent Events: live updates for the progress banner, the
  downloads card grid, and probe logs without full re-renders.
- Playlist expansion into per-video queue items, with approve-before-persist.
- Download de-duplication: URLs already present as `Done` are surfaced rather
  than silently skipped.
- `--cookies-from-browser` flag (optional, no default) and a configurable
  inter-download sleep timer (default 20s).
- `--addr`/`--bind` unification, `--timeout` flag, and graceful shutdown wired
  into the probe SSE stream so Ctrl+C stops the server cleanly.
- Askama-based HTML templates (replacing `format!` string literals), formatted
  with `djhtml`.
- Vendored `htmx` + SSE extension (pinned, non-minified) and a dark slate UI
  theme with pastel highlights.
- `README.md`, `docs/ARCHITECTURE.md`, MIT `LICENSE`, and full `Cargo.toml`
  metadata (description, keywords, categories).
- `format.sh` and `publish.sh` pre-publish gate plumbing `tests/` into the
  publish flow; ad-hoc validation scripts persisted under `tests/`.
- `systemd` user service file for running the server.
- Site favicon derived from `download.svg`.

### Changed
- Filename capture now uses `--print-to-file after_move` instead of log
  scraping; the worker parses `[Merger]` lines for the final remuxed name.
- Static and dynamic route cache policies tightened.
- `yt-dlp` defaults prefer `mp4` + 1080p with fallback to best available.
- Queue persisted as one JSON file per URL.

### Fixed
- Empty SSE payloads not dispatching in the browser.
- Stale frame-count sets and orphaned thumbnails garbage-collected; missing
  video state files pruned.

### Internal
- `e2e`/shell tests (network-dependent) are disabled in the default publish
  gate; `probe-flat-playlist.sh` requires a logged-in Firefox profile and is
  run by the operator.

## [0.1.0] - baseline

Unpublished development baseline. See `git log` for the full history.

[0.2.0]: https://github.com/mason-larobina/yt-downloader-webui/releases/tag/v0.2.0
[0.1.0]: https://github.com/mason-larobina/yt-downloader-webui/releases/tag/v0.1.0
