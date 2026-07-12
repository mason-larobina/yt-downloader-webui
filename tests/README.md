# web-dl test scripts

These are **not** `cargo test` unit tests. They are standalone shell scripts
written during validation of DESIGN.md and kept here so they can be re-run on
demand (e.g. after touching the worker, server, or a new yt-dlp version) or
forked as starting points for new scenarios.

Each script `set -euo pipefail`s, builds the release binary if needed, spins
up an isolated server in a temp dir, and cleans up after itself.

## Scripts

| Script                       | What it checks                                                                 |
|------------------------------|--------------------------------------------------------------------------------|
| `probe-ytdlp-flags.sh`       | yt-dlp `--newline --progress-template '%(progress)j'` output shape (the DESIGN Sec. 11 open question). Confirms one `\n`-terminated JSON object per tick, stdout/stderr split, `ERROR:` on failure. |
| `e2e-download.sh`            | Full happy path: POST a URL, watch SSE progress/queue/log/library, file lands in the download dir, item reaches `done`, final status is idle. |
| `persistence-restart.sh`     | SIGTERM mid-download -> item saved as `pending` -> on restart the worker re-starts it to `done`. Verifies shutdown ordering (worker exits before the final flush). |
| `endpoints.sh`               | Queue controls (`/download`, `/cancel`, `/retry`, `/clear`), file serving (inline/download/range), `/delete`, path-traversal guards, `ERROR:` capture into `item.error`. |

## Usage

```sh
# from the repo root, after `cargo build --release`:
./tests/e2e-download.sh
./tests/persistence-restart.sh
./tests/endpoints.sh
./tests/probe-ytdlp-flags.sh

# override the URL (e.g. a different/smaller source):
./tests/e2e-download.sh 'https://example.com/video.mp4'

# override port / binary / SSE capture window:
WEB_DL_BINARY=./target/release/web-dl PORT=18090 TIMEOUT=120 ./tests/e2e-download.sh
```

## Notes

- `e2e-download.sh` and `persistence-restart.sh` hit the network (archive.org
  Big Buck Bunny by default). Network speed varies; bump `TIMEOUT` if the SSE
  window closes before completion.
- `endpoints.sh` uses an unavailable YouTube URL to exercise the failure path;
  it needs no real download.
- `probe-ytdlp-flags.sh` requires `yt-dlp` on `PATH`.
- Unit tests (parser, no network) live in `src/parse.rs` and run via
  `cargo test`.
