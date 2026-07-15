# yt-downloader-webui test scripts

These are **not** `cargo test` unit tests. They are standalone shell scripts written during validation of ARCHITECTURE.md and kept here so they can be re-run on demand (e.g. after touching the worker, server, or a new yt-dlp version) or forked as starting points for new scenarios.

Each script `set -euo pipefail`s, builds the release binary if needed, spins up an isolated server in a temp dir, and cleans up after itself.

## Scripts

| Script | What it checks | |------------------------------|--------------------------------------------------------------------------------| | `probe-ytdlp-flags.sh` | yt-dlp `--newline --progress-template '%(progress)j'` output shape (the ARCHITECTURE Sec. 11 open question). Confirms one `\n`-terminated JSON object per tick, stdout/stderr split, `ERROR:` on failure. | | `probe-flat-playlist.sh` | yt-dlp `--flat-playlist -j` info-extraction shape (no download), validated against a real YouTube playlist with Firefox cookies. Confirms each entry's `url` is already the full watch URL, the streaming `-j` line carries `playlist_index`/`playlist_count`/`playlist_title`, and a single video under `--flat-playlist` is a full 623 KB extraction (so single videos are not free to probe). Saved artifacts under `tests/yt-downloader-webui-probe-flat.*` feed the parser unit tests in `src/parse.rs`. | | `e2e-download.sh` | Full happy path: POST a URL, watch SSE progress/queue/log/library, file lands in the download dir, item reaches `done`, final status is idle. | | `persistence-restart.sh` | SIGTERM mid-download -> item saved as `pending` -> on restart the worker re-starts it to `done`. Verifies shutdown ordering (worker exits before the final flush). | | `endpoints.sh` | Queue controls (`/download` -> probe-area shell, GET `/probe` SSE result, POST `/confirm`, `/cancel`, `/retry`), file serving (inline/download/range), `/delete`, path-traversal guards, probe-error -> result event (no queue item), cancel+retry cycle. |

## Usage

```sh
# from the repo root, after `cargo build --release`:
./tests/e2e-download.sh
./tests/persistence-restart.sh
./tests/endpoints.sh
./tests/probe-ytdlp-flags.sh
./tests/probe-flat-playlist.sh   # run on a machine with a logged-in Firefox

# override the URL (e.g. a different/smaller source):
./tests/e2e-download.sh 'https://example.com/video.mp4'

# override port / binary / SSE capture window. NOTE: the binary env var is
# YT_DOWNLOADER_WEBUI_BINARY (the scripts build release themselves if unset):
YT_DOWNLOADER_WEBUI_BINARY=./target/release/yt-downloader-webui PORT=18090 TIMEOUT=120 ./tests/e2e-download.sh
```

## Notes

- The four network-safe scripts (`endpoints.sh`, `e2e-download.sh`,
  `persistence-restart.sh`, `probe-ytdlp-flags.sh`) are run automatically by
  `publish.sh` before every `cargo publish`, against a single prebuilt release
  binary (`YT_DOWNLOADER_WEBUI_BINARY=target/release/yt-downloader-webui`).
  `probe-flat-playlist.sh` is **not** run by `publish.sh` (it needs Firefox) —
  run it by hand on a machine with a logged-in profile.

- `e2e-download.sh` and `persistence-restart.sh` hit the network (archive.org Big Buck Bunny by default). Network speed varies; bump `TIMEOUT` if the SSE window closes before completion.
- `endpoints.sh` uses a nonexistent YouTube video ID to exercise the probe failure path (error returned as a `result` event with an error + Done button, nothing enqueued); it also probes+confirms a real archive.org download to drive the cancel+retry cycle. It needs no cookies.
- `probe-ytdlp-flags.sh` requires `yt-dlp` on `PATH`.
- `probe-flat-playlist.sh` requires `yt-dlp` on `PATH` **and a logged-in Firefox profile** (`--cookies-from-browser firefox`); YouTube 403s anonymous, cookie-less requests, so it must be run on the operator's own machine, not in a sandbox without cookies. It writes artifacts to a temp dir and prints the path; paste the output (or the saved JSON) back to pin the parser to the real shape.
- Unit tests (parser + approval-list JSON round-trip, no network) live in `src/parse.rs` and `src/render.rs` and run via `cargo test`.
