# yt-downloader-webui

A standalone, single-binary web wrapper around [`yt-dlp`][ytdlp]. Paste a video URL into the header, it gets queued, and a single background worker downloads it while you watch live progress in the browser. Pasting a playlist URL probes it and presents the per-video entries for you to pick from. Runs against your own home directory, reusing fresh cookies from your local Firefox profile by default so age-restricted / members-only content works.

- **Single self-contained binary.** All HTML, CSS, JS, and templates are embedded at compile time — no external file dependencies at runtime, no CDN.
- **htmx + SSE** for live updates: the queue, the active download's progress, and the yt-dlp log output are all pushed to every open tab in real time.
- **One worker, one `yt-dlp` at a time.** Submitted URLs are appended to a global queue; a single background worker drains it. Submitting while a download is in flight never rejects — it just enqueues.
- **Global, shared state.** Open the app in a second tab and you see the same queue and the same active download. Still single-user, no auth — a local loopback tool.
- **Queue survives restarts.** Pending and active items are persisted to a JSON file per item; on restart an interrupted download is re-queued.
- **Playlist support.** Pasting a playlist URL probes it (without downloading) and presents the per-video entries as confirm cards — tick a subset to enqueue only those. A single video is enqueued directly.
- **Downloads grid.** Each queued item is a card with open / download / delete actions once it finishes. The motivating use case: submit a URL from a phone, let the server fetch it with home cookies, then tap a card's *download* to pull the finished file onto the phone.
- **Thumbnails.** Fetched during the probe (or generated from the downloaded file with ffmpeg as a fallback) and cached on disk.

## Prerequisites

- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH` (or pass `--yt-dlp`).
- [`ffmpeg`](https://ffmpeg.org/) on `PATH` (or pass `--ffmpeg`). Optional — only used to generate thumbnails when the probe found none; a missing ffmpeg is a warning, not fatal.
- A browser profile for `--cookies-from-browser` (e.g. `firefox`), **optional**. Omit the flag to run without cookies; otherwise pass a browser name and yt-dlp reads its cookies on every invocation.

## Build

```sh
cargo build --release
# binary: target/release/yt-downloader-webui
```

## Run

```sh
cargo run --release
# listens on http://127.0.0.1:8080
```

Open <http://127.0.0.1:8080>, paste a URL (or just Ctrl/Cmd+V anywhere on the page — the input catches the paste and submits immediately), and watch it go.

### CLI flags

```
yt-downloader-webui [OPTIONS]

Options:
  -d, --download-dir <DIR>            Where yt-dlp writes files. Default: ~/Downloads
  -b, --cookies-from-browser <BROWSER> Browser cookies via --cookies-from-browser.
                                       Optional; forwarded to yt-dlp only when set.
                                       Omit to disable.
      --yt-dlp <PATH>                  Path to yt-dlp binary. Default: yt-dlp (PATH).
      --ffmpeg <PATH>                  Path to ffmpeg (thumbnail fallback). Default: ffmpeg.
      --state-dir <DIR>                 Queue persistence dir (one JSON per item).
                                       Default: ~/.local/share/yt-downloader-webui/queue/
      --cache-dir <DIR>                 Thumbnail cache dir (safe to clear).
                                       Default: ~/.cache/yt-downloader-webui/thumbs/
      --bind <ADDR>                    Bind address. Default: 127.0.0.1:8080.
                                       0.0.0.0:<port> is DANGEROUS — prints a warning.
      --sleep <SECONDS>                Sleep between consecutive downloads (rate-limit).
                                       0 disables. Default: 20.
  -v, --verbose                        Verbose logs (yt_downloader_webui=debug).
      --timeout <SECONDS>              Shut down after N seconds (testing aid).
  -h, --help                           Print help.
  -V, --version                        Print version.
```

`~` is expanded against `$HOME`. `RUST_LOG` is honoured when set; otherwise the default filter is `info` (`-v` bumps to `yt_downloader_webui=debug`). Under systemd, logs go to the journal with native priorities (`journalctl -p warning`); otherwise a human-readable stderr formatter is used.

## How it works

### Probe → confirm → download

1. **Paste a URL.** The header form posts to `/download`, which swaps in a probe area wired to its own SSE stream (`/probe?url=…`).
1. **Probe.** `yt-dlp --flat-playlist -j` classifies the URL *without downloading anything* (extraction only, fast). Each output line is streamed live to the probe area.
   - A **playlist** yields N entries — each entry's `url` is already the full per-video watch URL. They're rendered as confirm cards with checkboxes; tick a subset and *Confirm* to enqueue only those.
   - A **single video** yields one video dict. It's enqueued directly for download (using the original submitted URL, not the media URL the probe returned).
   - A **failure** (e.g. `ERROR: Video unavailable`) renders an error line and a *Done* button that restores the header.
1. **Confirm** (`/confirm`) enqueues the approved items, kicks off background thumbnail fetches, and restores the header for the next URL.
1. **Download.** A single background worker pops the next pending item, spawns one `yt-dlp` process with `--progress-template '%(progress)j'`, parses its structured progress JSON, and pushes live updates to every connected tab. Format selection prefers mp4 video ≤1080p + m4a audio, falling back through progressively looser selectors so it always grabs something rather than failing. `--merge-output-format mp4` keeps merged containers mp4.

The probe runs concurrently with downloads (it writes no files), so you can paste the next URL while one is in flight. A probe dropped by the client (navigate away, cancel) kills the `yt-dlp` child via `kill_on_drop`.

### The queue

A single global FIFO queue holds every submitted item. Items move through `pending → active → done | failed | cancelled`. The queue view renders cards newest-first; the floating banner shows the active download's thumbnail, title, progress bar, ETA, speed, and a live pending count. Every connected tab sees the same state via the `/events` SSE stream (a snapshot is sent on connect, then live events are forwarded).

Items can be cancelled (pending removal or killing the active download), retried (re-enqueued at the back), or cleared (terminal items dropped). Done items dedupe in two places, both visibly: re-submitting a URL whose previous item is `done` re-surfaces that item to the top of the grid (it keeps its state, no duplicate row or file is created) and shows a short toast, so re-downloading a playlist doesn't create duplicate files -- and isn't silently swallowed. When a finished download lands on a filename already owned by an older `Done` item (a re-download of a file already on disk via a different URL, or a stale duplicate state file), the reconcile pass keeps the **newest** item -- so the card you were watching stays put -- subsumes the older item's state (url / title / duration / media / thumbnails) into it, drops the older item, and shows a short toast so the de-duplication isn't silently buried. A per-item details page (`/item/:id`) shows the full thumbnail, metadata, and the complete yt-dlp log output (polled while the download may still be producing lines).

### Persistence

Each item is written to its own `<unix_ts>.json` file in the state dir (atomic `.tmp` + fsync + rename). On startup every file is loaded, ordered FIFO by enqueue time, and any item previously `active` is re-queued as `pending` so an interrupted download restarts. A corrupted file is moved aside to `<name>.bad-<ts>` and skipped — the rest of the queue still loads. The state dir is reconciled to mirror the live queue: cleared / history-trimmed items' files are deleted. Per-item yt-dlp logs are persisted too, so they survive a restart.

### File serving

Finished downloads expose open / download / delete actions right on their card in the grid. `/file/:name` streams a file (inline for in-browser preview, or as an attachment) with single-range support for media seeking on mobile. Filenames are percent-encoded in links and path-traversal-guarded on disk.

A **rescan** button in the downloads header re-runs the download-directory reconcile (the same pass that runs on startup and after each download): it prunes state for files removed out-of-band, imports newly-added videos as cards, and re-probes items missing media info. The grid refreshes automatically once the rescan completes.

## Security

This is a **local, single-user tool** bound to `127.0.0.1` by default. It runs `yt-dlp` with your browser cookies and can read/delete files in your download directory. **Do not expose it to untrusted networks.** Binding `0.0.0.0` prints a loud warning; if you need mobile/remote access, reach the loopback server via a tunnel (Tailscale, SSH port-forward) rather than binding all interfaces.

## Project layout

```
src/
  main.rs       Entry point: CLI → Config → load queue → start server
  config.rs     CLI parsing + resolved Config
  server.rs     Axum router, route handlers, the /events SSE stream
  worker.rs     The single background worker: drains the queue, drives yt-dlp
  ytdlp.rs      Builds the yt-dlp Command (download + probe)
  parse.rs      Classify a yt-dlp output line (progress JSON vs log)
  state.rs      AppState, Queue, QueueItem, Progress
  persist.rs    Per-item JSON persistence (one file per item)
  library.rs    Download-dir scan, file streaming, delete
  thumb.rs      Thumbnail fetch + ffmpeg generation, cache
  render.rs     Server-side HTML fragments (askama templates) for SSE events
  events.rs     Event types + the log ring buffer
templates/      askama HTML templates
static/         index.html, app.css, icons, vendored htmx + SSE extension
tests/          end-to-end shell scripts
docs/ARCHITECTURE.md  Architecture notes (may lag the implementation)
```

## License

MIT.

[ytdlp]: https://github.com/yt-dlp/yt-dlp
