# DESIGN.md -- `web-dl`

A standalone, single-binary web wrapper around `yt-dlp` for personal use. You
paste one or more video URLs into a textarea, hit *Download*, and they are
**appended to a global queue**. A single background worker drains the queue
one URL at a time -- there is only ever one `yt-dlp` process running -- and you
watch its progress live in the browser. The queue and live status are **shared
across all sessions/tabs**: open the app in a second tab and you see the same
queue and the same active download. Runs against the user's own home
directory, using fresh cookies from the local Firefox profile by default.

---

## 1. Goals & non-goals

**Goals**
- Single self-contained binary; all HTML/CSS/JS/templates embedded (no
  external file dependencies at runtime, no CDN).
- Frontend uses **htmx** (plus the htmx SSE extension for live output).
- One large textarea; paste N line-separated URLs; each URL is **appended to
  a global queue** (in-memory at runtime, **persisted to a simple JSON file** so
  it survives restarts -- see Sec. 8). A single background worker drains the
  queue **one URL at a time**, spawning one `yt-dlp` process per item. There is
  only ever one yt-dlp process running at any moment; submitting while a
  download is in flight never rejects -- it just enqueues.
- **Queue and live status are global and shared across all sessions/tabs.**
  Every connected browser sees the same queue and the same active-item
  progress. Still single-user, no auth (see Sec. 9).
- Live, readable progress rendered from yt-dlp's structured progress output
  -- not the raw `\r`-redrawn shell status line.
- Fresh cookies pulled from the home Firefox profile by default
  (`--cookies-from-browser firefox`).
- Configurable download directory via a flag, defaulting to `~/Downloads`.
- **Library view** of all files in the download directory (assumed to have been
  produced by the tool). Rendered as a simple list with `[open]` (play/preview
  inline in the browser) and `[download]` (save to the client device) buttons per
  file -- plus `[delete]`. The motivating use case: submit a URL from a phone,
  let the server fetch it with home cookies, then tap `[download]` on the
  completed file to pull it onto the phone. See Sec. 6/7.

**Non-goals**
- Multi-user / auth / remote access. This is a local single-user tool bound to
  `127.0.0.1`. (See Sec. 9 Security.) Multi-*session*/multi-tab is supported and
  is a first-class goal above; multi-*user* is not. Mobile/phone use is
  supported but expects the operator to reach the loopback server via a tunnel
  (Tailscale / SSH port-forward) or to opt into `--bind 0.0.0.0` (Sec. 9).
- A rich media library (transcoding, tagging, search, thumbnails). The library
  is a flat file list with open/download/delete; no metadata DB. (Queue *does*
  persist across restarts -- see Sec. 8 -- so a restart no longer drops
  pending/active items.)
- Supporting browsers/cookie stores other than Firefox *by default* (a flag
  can override, but Firefox is the blessed path).

---

## 2. Stack

| Layer        | Choice                                              |
|--------------|-----------------------------------------------------|
| Language     | Rust (edition 2024) -- per existing `Cargo.toml`    |
| Runtime      | `tokio` (multi-thread)                              |
| Cancel/sync  | `tokio_util::sync::CancellationToken`              |
| HTTP server  | `axum` (good SSE support, pairs well with htmx)     |
| CLI parsing  | `clap` (derive)                                     |
| JSON         | `serde_json`                                        |
| Serde        | `serde` (derive)                                    |
| Time         | `time` (serde feature, for persisted timestamps)   |
| Errors       | `anyhow` (app) + `thiserror` (library-ish types)    |
| Logging      | `tracing` + `tracing-subscriber` + `tracing-journald` |
| Templates    | none needed -- the page is one static `index.html`   |
| Static embed | `include_str!` / `include_bytes!` of `static/`    |
| Frontend     | htmx core + `htmx-ext-sse` (vendored, embedded)     |

The index page is small and fixed, so a template engine is overkill. We embed
`index.html`, `htmx.min.js`, `htmx-ext-sse.js`, and a small `app.css` directly
into the binary with `include_bytes!`/`include_str!` and serve them from `/static/*`
and `/`.

### Why async at all

This is a single-user tool used a handful of times a week with 1-2 open tabs,
so async is *not* a workload necessity: the worker runs one yt-dlp at a time
(blocking line reads), and 1-2 SSE connections are 1-2 parked threads -- a
thread-per-connection model would handle it fine. We keep `tokio`/`axum` anyway
because (a) axum's SSE is ~5 lines and the Rust web ecosystem makes the async
path the path of least resistance, and (b) cancel-via-`select!` on a
`CancellationToken` (Sec. 8) is cleaner than a polled `AtomicBool`. The cost --
a runtime and `Send`/`Sync` reasoning for a rarely-used binary -- is accepted as
a one-time convenience tax.

---

## 3. CLI

```
web-dl [OPTIONS]

  -d, --download-dir <DIR>     Where yt-dlp writes files (passed as -P).
                               Default: ~/Downloads
  -b, --cookies-from-browser <B>  Browser to pull cookies from via
                               --cookies-from-browser (matches the yt-dlp
                               flag). Default: firefox. Use "none" to
                               disable cookies entirely.
      --yt-dlp <PATH>          Path to yt-dlp binary. Default: yt-dlp (PATH).
      --state-file <PATH>      Queue persistence file. Default:
                               ~/.local/share/web-dl/queue.json. Resolved
                               via the home/dir crate; parent dir created.
                               Set to a tmpfs path for non-persistence.
      --bind <ADDR>             Bind address (host:port). Default:
                               127.0.0.1:8080 (loopback). Use
                               0.0.0.0:<port> to listen on all
                               interfaces -- DANGEROUS; prints a warning.
                               See Sec. 9.
  -v, --verbose                Verbose server logs. Bumps the `EnvFilter` to
                               `web_dl=debug,info`; `RUST_LOG` is honored if
                               set explicitly.
```

At startup the app runs `yt-dlp --version`; if it is missing or fails it exits
with a clear, actionable message (install yt-dlp / fix `--yt-dlp`).

`tracing_subscriber` is installed at startup with an `EnvFilter` (defaulting
to `info`, bumping to `web_dl=debug,info` under `-v`, and honoring `RUST_LOG`
when set explicitly). When journald is reachable (i.e. running under systemd)
logs go to the journal via `tracing-journald` with **native priorities**, so
`journalctl -p err` / `-p warning` filter by level; otherwise it falls back to
a human-readable stderr formatter (ANSI only when stderr is a terminal, so
escape codes never land in the journal or a redirected log file). Server
request/error logs, yt-dlp lifecycle messages, and the `--bind` warning all
flow through `tracing::info!`/`warn!`. The `--bind` warning is additionally
printed to stderr before binding so it is visible even if the subscriber
failed to install. For a `systemd --user` unit, set `SyslogIdentifier=web-dl`
and `StandardError=journal` (the latter is the default for user units).

`~/Downloads` is resolved via the `HOME` env var (falling back to the process's
home as reported by the `home`/`dirs` crate). The resolved path is created if
missing. The `--state-file` default (`~/.local/share/web-dl/queue.json`) is
resolved the same way; its parent directory is created at startup.

Before serving, the app **loads `queue.json`** (Sec. 8) and reconstructs the
in-memory queue: pending items are re-queued, any item left `Active` (a crash)
becomes `Pending` to be re-started, and done/failed/cancelled items are kept
as history. The worker is then notified if anything is pending. If the file is
missing it starts empty; if it fails to parse it is moved aside to
`queue.json.bad-<ts>` and a fresh empty queue starts, with a `warn!` log.

---

## 4. yt-dlp invocation

The background worker pops **one queued URL at a time** and runs one
`yt-dlp` process for it. When that process exits (success or error), the
worker marks the queue item done/failed and pops the next pending item; if the
queue is empty it parks until a `POST /download` enqueues something. There is
never more than one live yt-dlp process.

```
yt-dlp \
  --cookies-from-browser firefox \
  --newline \
  --progress-template '%(progress)j' \
  -P <download-dir> \
  <URL>
```

Flag rationale:

- `--cookies-from-browser firefox` -- reads fresh cookies from the home Firefox
  profile on every invocation (yt-dlp copies `cookies.sqlite`, so it works even
  while Firefox is open). Satisfies the "fresh cookies from home Firefox
  profile" requirement with zero configuration.
- `--newline` -- forces each progress tick onto its own line instead of being
  redrawn with `\r`. Combined with the template below this gives us
  newline-terminated JSON chunks, one per tick, trivially line-parseable.
- `--progress-template '%(progress)j'` -- replaces the human progress bar with a
  JSON dump of the progress dict per tick. Fields we care about:
  `status`, `filename`, `tmpfilename`, `downloaded_bytes`, `total_bytes`,
  `total_bytes_estimate`, `speed`, `eta`, `elapsed`, `fragment_index`,
  `fragment_count`, plus yt-dlp's preformatted `_percent_str`, `_speed_str`,
  `_eta_str`, etc. when we want them.
- `-P <dir>` -- sets the output *directory* (per yt-dlp's `paths` option),
  leaving yt-dlp's default filename template (`-o`) intact. A bare directory as
  the `--paths` value is the documented supported form and composes with the
  default output template, so files land directly in `<download-dir>` with
  yt-dlp's chosen name.

The single URL is passed as one `Command::arg(...)` value -- **no shell**, so
URL contents can never cause injection.

stderr is captured alongside stdout and merged into the log stream as
warning/error lines (stderr is where yt-dlp prints `WARNING:` and `ERROR:`).

### Why one process per queued item

Driving the queue ourselves (one yt-dlp invocation = one queue item) means the
server is the source of truth for queue identity, ordering, and per-item
state: each item gets a stable id at enqueue time, and we mark it
pending/active/done/failed directly from the process lifecycle plus the
progress JSON. We do not depend on yt-dlp's `[download] Downloading video N of
M` framing, and a URL pasted while another is downloading simply waits its
turn -- no batch boundaries to reason about, and no risk of two processes
running if two tabs submit concurrently.

---

## 5. Parsing yt-dlp output

stdout/stderr are read line-by-line via `tokio::io::BufReader`. Each line is
classified:

| Line shape                                             | Handling                                         |
|--------------------------------------------------------|--------------------------------------------------|
| Parses as JSON object with a `status` field           | **Progress event** -> update active queue item.  |
| `[download] Destination: <file>` or `tmpfilename`/`filename` in progress JSON | Set current item's filename. |
| Progress `status == "finished"`                       | Mark active queue item +; worker pops next.     |
| Progress `status == "error"`                           | Mark active item x, surface error in log; worker pops next. |
| Anything else (stderr `WARNING:`/`ERROR:` included)   | **Log event** -> append to scrollback + ring buffer. |

The server owns the queue (ids, ordering, per-item state); yt-dlp output only
updates the *active* item. Progress JSON is the source of truth for the active
item's percent/speed/ETA; plain-text log lines populate the scrollback and the
ring buffer replayed to newly-connected tabs (see Sec. 8).

### Throttling

yt-dlp emits progress ticks roughly 10-20x/s. The server **throttles progress
events** to ~5/sec (emit at most one every 200 ms, always emitting the final
`finished`/`error` state). This keeps SSE traffic and DOM swaps sane; htmx
would otherwise re-render the status fragment on every tick.

---

## 6. Request flow & htmx wiring

```
GET  /            -> index.html (static: textarea form + live output pane + library)
GET  /static/*    -> embedded htmx.js, sse ext, css
POST /download    -> append URLs to global queue; return ack fragment
POST /cancel/:id  -> cancel the active (or pending) queue item
POST /retry/:id   -> re-enqueue a cancelled/failed item at the back
POST /clear       -> drop all done/failed/cancelled items from the queue
GET  /library     -> render the current file list as an HTML fragment (htmx)
GET  /file/:name  -> stream a file from the download dir (inline or attachment)
POST /delete/:name -> delete a file from the download dir
GET  /events      -> long-lived global SSE: snapshot on connect, then live updates
```

### POST /download
1. Parse form body (`application/x-www-form-urlencoded`): the `urls` textarea.
2. Split on newlines, trim, drop blanks -> `Vec<String>` of URLs.
3. If empty -> return a small "paste at least one URL" ack fragment.
4. Otherwise: assign each URL a fresh id, append the items to the global queue
   (status = pending) under the queue lock, notify the worker, and return a
   small ack fragment (e.g. `<span id="ack">added 3 to queue</span>`) swapped
   near the submit button.

This handler never spawns yt-dlp and never rejects -- the background worker is
the only thing that runs yt-dlp. The textarea is cleared on success so the user
can paste the next batch. The real queue/status updates arrive over the SSE
connection the page opened on load (see GET /events); POST /download does not
touch that connection.

### POST /cancel/:id
Cancels the queue item with the given id. Behaviour depends on status:

- **Pending**: remove the item from the queue (it never ran) and emit a
  `queue` event.
- **Active**: set the item's `cancel` token (stored on the active `QueueItem`,
  Sec. 8). The worker, which is `select!`ing between yt-dlp output reads and
  `cancel.cancelled()`, wakes on the token, `child.kill()`s the process, marks
  the item `Cancelled`, emits a final `status` + `queue` event, and pops the
  next item.
- **Done/Failed/Cancelled**: no-op (return a small ack fragment saying so).

Returns a tiny ack fragment. This handler never spawns or signals yt-dlp
itself -- it only flips queue state and/or trips the token; the worker performs
the actual `kill()`. That keeps the single-process invariant intact.

### POST /retry/:id
Resets a `Failed` or `Cancelled` item to `Pending` (same id, cleared progress
+ error, moved to the back of the pending run), notifies the worker, and emits a
`queue` event. Returns an ack fragment. A `Done` item is rejected with an ack
("already downloaded"). Retrying the *currently active* item is rejected with
a suggestion to cancel it first.

### POST /clear
Removes all `Done`, `Failed`, and `Cancelled` items from `items` (pending +
active are always retained), then emits a `queue` event. Returns an ack
fragment like `<span id="ack">cleared 12 items</span>`.

### GET /library
Scans `cfg.download_dir` for regular files (non-recursive; subdirectories are
ignored) and renders an HTML fragment `<div id="library">...</div>` listing
them -- one row per file with name, size (human-readable), and mtime, plus
`[open]`, `[download]`, and `[delete]` controls. Returned as a fragment so it can
be swapped in by htmx on demand (the `[refresh]` button) or by a `library` SSE
event. The library is deliberately decoupled from `queue.json`: it reflects
*what exists on disk*, not *what was queued*, so files added or removed
out-of-band simply show up / disappear on the next scan. (See Sec. 11 for the
consequent drift.)

### GET /file/:name
Streams the named file from `cfg.download_dir`. `:name` must be a **bare
filename** (no `/`, no `..`); the path is `download_dir.join(name)`,
canonicalized, and asserted to still live under `download_dir` -- otherwise
404 (never an error that leaks whether a path outside the dir exists).

- Default (`?download=1` or the `[download]` button): `Content-Disposition:
  attachment; filename="<name>"` -- the browser saves it to the device (the
  mobile use case).
- `?inline=1` (or the `[open]` link): `Content-Disposition: inline` -- the
  browser plays/opens it in-tab.

Files are streamed with `Content-Length`/range support where practical (axum +
`tokio::fs`); large video files must not be buffered whole in memory.

### POST /delete/:name
Deletes the named file from `cfg.download_dir` (same path-traversal guard as
`/file/:name`), then emits a `library` event so every tab's list refreshes.
Returns a small ack fragment. **Irreversible** -- there is no trash/undo; the
client renders a `confirm("Delete <name>?")` guard via `hx-on::before-request`
before the POST fires. (See Sec. 9.)

### The output pane (in index.html)

`index.html` ships with the output pane already present and already wired to
SSE, so every tab is live from the moment it loads -- no fragment needed from
POST /download to "start" the stream:

```html
<div id="output" hx-ext="sse" sse-connect="/events">
  <div id="queue" sse-swap="queue" hx-swap="innerHTML"></div>
  <div id="status" sse-swap="status" hx-swap="outerHTML"></div>
  <div id="log">
    <div sse-swap="log" hx-swap="beforeend"></div>
  </div>
  <div id="library" sse-swap="library" hx-swap="innerHTML"></div>
</div>
```

### GET /events (SSE)
- Upgrades to a long-lived SSE response (`axum::response::sse::Sse` over a
  stream), one per connected tab. The connection is **global and app-lifetime**:
  not tied to any one item, and not closed when an item finishes.
- On connect, emit a `snapshot` event carrying the full current state -- the
  entire queue (pending + active + recent done/failed; see Sec. 8), the ring
  buffer of recent log lines, **and the current library file list** -- so a
  freshly opened or reconnected tab is immediately consistent with every other
  tab.
- Thereafter emit named events whose payloads are **HTML fragments**:
  - `status` -> a fresh `<div id="status">...progress bar...</div>` (replaces)
    for the active item; an idle "queue empty / waiting" status when the worker
    parks.
  - `log` -> a `<div class="logline">...escaped text...</div>` (appended).
  - `queue` -> the full queue list `<div id="queue">...</div>` (replaces),
    emitted on queue changes (items enqueued, item starts, item finishes).
  - `library` -> the full file list `<div id="library">...</div>` (replaces),
    emitted when a download finishes (new file appears) or a file is deleted.
    On a `finished` item the worker emits this alongside the final `queue`/
    `status` so the new file is immediately downloadable from any tab.
- Every fragment is HTML-escaped server-side (log lines especially -- they come
  from yt-dlp and may contain `&`, `<`, quotes).
- There is no per-job `done` event that closes the connection; per-item
  completion is just a `queue` swap. The connection closes only when the client
  disconnects (tab close) or the server shuts down.

Because each event carries a complete fragment and a target+swap, this stays
fully in the htmx model -- no hand-written rendering JS. (If high-frequency
`status` swaps prove visually janky, the fallback is a ~10-line vanilla
`EventSource` that updates a single element's `textContent`; see Sec. 11.)

---

## 7. UI

Single page, vertically stacked:

```
+----------------------------------------------+
| web-dl   -> ~/Downloads   cookies: firefox    |  <- header (dir, browser, link)
+----------------------------------------------+
| +------------------------------------------+ |
| | https://...                              | |  <- large <textarea>
| | https://...                              | |     (monospace, grows tall)
| |                                          | |
| +------------------------------------------+ |
|                                  [ Download ] |  <- submit (hx-post /download)
+----------------------------------------------+
| Now downloading: video1.webm                  |  <- #status (live progress bar)
| [##########----------] 73% | 2.1 MiB/s |     |
| ETA 00:12 | 7.3/10.0 MiB                     |
|                                               |
| queue (3):                                    |  <- #queue
|   + video1.webm                               |
|   ~ video2.webm (downloading)   [cancel]      |
|   . https://.../video3         (pending)      |
|   x video4.webm (failed)       [retry]        |
|   v video5.webm (done)          [download]    |  <- done row carries a file link
|                                  [ clear ]     |
|                                               |
| log:                                          |  <- #log (scrollback)
| [youtube] Extracting URL: ...                   |
| [download] Destination: video1.webm          |
| WARNING: ...                                    |
|                                               |
| library (12 files):                 [refresh]  |  <- #library
|   video1.webm   42 MiB   2026-07-11             |
|                [open]   [download]   [delete]  |
|   talk.mp4     120 MiB  2026-07-10            |
|                [open]   [download]   [delete]  |
+----------------------------------------------+
```

- Progress bar is a CSS bar (`<div class="bar"><i style="width:73%></i></div>`)
  generated server-side from progress JSON.
- `#log` is a scrollable `<pre>`-styled region; new lines appended at bottom;
  auto-scroll to bottom unless the user has scrolled up (simple: always
  auto-scroll for v1; an `hx-on`-ish scroll guard can come later).
- The submit button is always enabled -- POST /download just appends to the
  queue, so there is nothing to "wait out". htmx `hx-disabled-elt` is used only
  for the brief in-flight POST itself (re-enabled the instant the ack returns).
- Queue rows carry small action buttons rendered by the `queue` fragment:
  `[cancel]` on the active (and pending) item, `[retry]` on failed/cancelled
  items, a `[download]` link on `done` rows (pointing at `/file/:name?download=1`,
  so the just-finished file is one tap away on mobile), and a single
  `[ clear ]` button below the list that posts to `/clear`. Each button is an
  `hx-post`/link that targets the ack span near the submit button; the
  resulting state change arrives over SSE as a `queue` swap.
- A pending item displays its **URL** as its label until yt-dlp emits a
  `Destination:` (or a `filename` appears in the progress JSON); only the
  active item resolves to a real filename. This is why the mockup shows
  `video3` as a bare URL while the active row shows `video2.webm`.
- A second tab opened against the same server renders the identical queue,
  active status, and library (via the `snapshot` event on SSE connect); all tabs
  stay in lockstep via the same global event stream.
- `#library` rows: `[open]` links to `/file/:name?inline=1` (browser plays/
  opens in-tab), `[download]` links to `/file/:name?download=1` (saves to the
  device -- the mobile use case), `[delete]` is an `hx-post` to
  `/delete/:name` guarded by a client-side `confirm()`. A `[refresh]` button
  above the list does an `hx-get="/library"` swap for a manual re-scan (e.g.
  after dropping a file in via the file manager).

---

## 8. Server state

```rust
struct AppState {
    cfg: Config,                          // download_dir, browser, yt_dlp path, ...
    queue: Mutex<Queue>,                  // global; see below
    events: broadcast::Sender<Event>,     // app-lifetime; every /events client subscribes
    log_ring: Mutex<RingBuffer<String>>,  // recent log lines replayed on connect
    notify: Notify,                       // wakes the worker when items are enqueued
}

struct Queue {
    items: Vec<QueueItem>,    // pending + active + recently done/failed
    next_id: u64,             // monotonically increasing item id
}

struct QueueItem {
    id: u64,
    url: String,
    status: ItemStatus,       // Pending | Active | Done | Failed | Cancelled
    filename: Option<String>,
    progress: Option<Progress>, // percent/speed/eta/bytes from progress JSON
    error: Option<String>,
    cancel: Option<CancellationToken>, // set when Active; POST /cancel trips it
    enqueued_at: OffsetDateTime,      // wall-clock; serialized to RFC3339
}

enum ItemStatus { Pending, Active, Done, Failed, Cancelled }
```

The `broadcast::Sender<Event>` is created once at startup and lives for the
whole app -- it is **not** per-job. Every `/events` client holds a
`broadcast::Receiver`; multiple tabs all receive the same events. `broadcast`
(not `mpsc`) so a tab reconnecting mid-download still gets subsequent events,
and the `snapshot` event on connect (built from `queue` + `log_ring` under the
locks) makes it consistent with everyone else.

A single worker task is spawned at startup. Its loop:

1. Lock the queue; if there is a pending item, flip it to `Active`, mint a
   `CancellationToken`, stash it on the item, and take it; otherwise drop the
   lock and `notify.notified().await` until POST /download wakes it.
2. Spawn `yt-dlp` for that one URL. Read stdout/stderr line-by-line **raced**
   against `cancel.cancelled()` via `tokio::select!`, classify (Sec. 5), update
   the active `QueueItem` under the lock, push log lines into `log_ring`, and
   `events.send(...)` the throttled `status`/`log`/`queue` fragments.
   - On `cancel.cancelled()`: `child.kill().await`, drain any buffered output,
     mark the item `Cancelled`, emit a final `status` + `queue` event.
   - On clean exit (`status == "finished"` / exit code 0): mark `Done`.
   - On error (`status == "error"` or non-zero exit): mark `Failed`, surface
     the message in `error` and the log.
3. Clear the item's `cancel` token, emit a final `queue` (and final `status`)
   event, then loop to step 1.

Because only the worker ever spawns (or kills) yt-dlp, there is only ever one
live process. Concurrent POST /download handlers only mutate the queue and
`notify`; POST /cancel only trips a `CancellationToken`; POST /retry only
rewrites an item's status; POST /clear only drains terminal items.

Done/failed/cancelled items are kept in `items` (capped -- e.g. last 200) so the
queue view shows recent history; pending + active are always retained. The
`[ clear ]` button (Sec. 6 `/clear`) lets the operator drop the terminal items
explicitly in v1.

### Persistence (queue.json)

The queue is persisted to a single JSON file (path from `--state-file`, default
`~/.local/share/web-dl/queue.json`). The serialized form is a thin, stable
projection of the in-memory structs:

```json
{
  "version": 1,
  "next_id": 42,
  "items": [
    { "id": 7, "url": "https://...",
      "status": "pending",
      "filename": null,
      "error": null,
      "enqueued_at": "2026-07-11T22:54:01Z" },
    { "id": 6, "url": "https://...",
      "status": "done",
      "filename": "video.webm",
      "error": null,
      "enqueued_at": "2026-07-11T22:50:33Z" }
  ]
}
```

`progress` and `cancel` are **runtime-only** and never serialized; on restart
`progress` is `None` until the worker re-runs the item.

**Write strategy.** The file is rewritten on every *structural* queue
mutation -- enqueue, status transition (start/finish/fail/cancel), retry,
clear, and the startup requeue. It is **not** written on progress ticks
(throttled to ~5/s already, but transient by nature and meaningless across
restarts). Writes are atomic: serialize to `<file>.tmp`, `fsync`, then rename
over the target, so a crash mid-write leaves either the previous or the new
version, never a truncated half-file. There is no separate write thread --
the mutation already holds the queue lock, so it serializes, serializes, and
renames inline; at this workload (a few writes per session) the cost is
negligible. A final flush is performed during shutdown (below).

**Load / restart semantics on next startup:**

| Stored status | On load becomes | Re-started? |
|---------------|----------------|-------------|
| `pending`     | `pending`      | yes (worker picks it up) |
| `active`      | `pending`      | yes -- the process is gone; re-queue |
| `done`        | `done`         | no (history) |
| `failed`      | `failed`       | no (history; use `[ retry ]` to re-run) |
| `cancelled`   | `cancelled`   | no (history; use `[ retry ]` to re-run) |

So an `active` item left in the file by a crash is transparently re-started on
next launch; an item the user explicitly `cancelled` (or that `failed`) is kept
as history and only re-runs if the user hits `[ retry ]`.

### Shutdown

On `SIGINT`/`SIGTERM` (via `tokio::signal`) the server performs a graceful
shutdown:

1. Stop accepting new connections (axum's `with_graceful_shutdown`).
2. Trip a **global `shutdown` `CancellationToken`**; the worker's per-item
   `select!` additionally waits on this token, so the active yt-dlp is killed
   (`child.kill()`). The active item is then marked **`Pending`** (not
   `Cancelled`) so it is **re-started on next launch** (see Persistence above)
   -- shutting the server down is not treated as an intent to abandon the
   download; the user can hit `[ cancel ]` first if they truly want it dropped.
3. The worker drains: after killing the active child it flips the item to
   `Pending`, emits the final `status`/`queue` events, and exits.
4. In-flight `/events` SSE streams are dropped (each `Sse` stream is selected
   against the same shutdown token so connections close promptly rather than
   hanging).
5. The queue is **flushed to `queue.json`** one last time (atomic write) so
   every pending item and the re-queued active item survive. `pending` items
   are kept as-is; `active` is now `pending`; a `warn!` log reports how many
   items are queued for re-start.

The global `shutdown` token is stored on `AppState` alongside `notify`; it is
the same token the per-item cancel token is *also* wired to, so cancelling an
item and shutting the server down share one code path.

---

## 9. Security

- **Bind loopback only by default.** `--bind` defaults to `127.0.0.1:8080`; any
  non-loopback value (e.g. `--bind 0.0.0.0:8080`) prints a loud warning. Anyone who
  can reach the server can run `yt-dlp` against arbitrary URLs (limited to
  what `--cookies-from-browser firefox` allows), read live download status,
  **download any file in your download dir to their device, and delete files**
  (see `/file/:name`, `/delete/:name` below) -- i.e. effectively act as your
  Firefox session for these sites *and* as a file server for that directory.
  Do not expose to a network. **Mobile use** requires reaching the loopback
  server: prefer a tunnel (Tailscale / SSH port-forward) so the surface stays
  authenticated/encrypted; `--bind 0.0.0.0:<port>` is the escape hatch and prints its
  warning at startup.
- **No shell.** URLs are `Command::arg`s, never concatenated into a shell
  string -> no command injection.
- **HTML-escape every fragment** sent over SSE; log lines are untrusted text.
- **Path traversal guarded, not absent.** `/file/:name` and `/delete/:name`
  *do* let the browser name a file, but `:name` must be a bare filename (no `/`,
  no `..`); the resolved path is canonicalized and asserted to remain inside
  `cfg.download_dir`, otherwise 404. The browser cannot escape the download
  directory -- only read/delete files already within it. `--download-dir`
  itself remains a CLI flag set by the operator.
- **Delete is irreversible.** `/delete/:name` removes the file from disk with
  no trash/undo; the UI guards with a `confirm()` but the server performs the
  removal immediately. This is acceptable for a single-user loopback tool and
  is the reason reaching the server is treated as full trust above.
- Cookies never leave the machine; yt-dlp reads them locally.

---

## 10. Project layout

```
web-dl/
+-- Cargo.toml
+-- DESIGN.md
+-- src/
|   +-- main.rs           // clap CLI -> Config -> start server
|   +-- config.rs         // Config, defaults (~ expansion, addr, browser)
|   +-- state.rs          // AppState, Queue, QueueItem, ItemStatus
|   +-- persist.rs        // load/save queue.json (atomic write, restart requeue)
|   +-- library.rs        // scan download_dir, serve /file/:name, /delete/:name
|   +-- worker.rs         // single background worker loop (drain queue, run yt-dlp)
|   +-- server.rs         // axum routes: / , /static, /download, /events
|   +-- ytdlp.rs          // build Command, spawn, line reader, parse
|   +-- parse.rs          // classify lines -> Event (progress / log)
|   +-- render.rs         // Event -> HTML fragment (escape, progress bar, queue)
|   +-- events.rs         // Event enum + throttling + ring buffer
+-- static/               // embedded at build time
    +-- index.html
    +-- app.css
    +-- htmx.min.js        // vendored, pinned version
    +-- htmx-ext-sse.js    // vendored, pinned version
```

`include_bytes!("static/htmx.min.js")` etc. keep everything inside the binary.
Vendored htmx files are committed to the repo (pinned, with a `VERSION` note).

---

## 11. Risks / open questions

- **`--progress-template '%(progress)j'` exact semantics.** **RESOLVED by validation against yt-dlp 2026.07.04.** Confirmed that `%(progress)j` emits exactly one JSON object per progress tick, and that `--newline` makes each tick `\n`-terminated (no `\r`), so line-based parsing in `parse.rs` works as designed. stdout carries the progress JSON plus `[generic]`/`[info]`/`[download] Destination:` info lines; stderr carries `WARNING:`/`ERROR:`. The merged stdout+stderr line stream (Sec. 4) is therefore correct. One nuance surfaced and is handled: yt-dlp prints a `status: "error"` progress JSON only for *mid-download* aborts; extraction/format failures (e.g. "Video unavailable") print `ERROR: ...` to stderr and exit non-zero with no error progress event. The worker harvests `ERROR:` lines into `item.error` so the queue row surfaces the real reason rather than a generic "exited with status 1".
- **SSE swap churn (decided).** Status/queue/log are rendered via htmx
  `sse-swap` fragments from the start (Sec. 6). If re-rendering the `#status`
  fragment ~5x/s proves visually janky in practice, the fallback is a ~10-line
  vanilla `EventSource` + `textContent` update for `#status`/`#queue` only,
  keeping htmx for the form and `#log` appends. This is a localized change in
  `index.html` + a few lines of JS, not an architectural shift, and will only
  be taken if observed, not preemptively.
- **Firefox profile lock.** Modern yt-dlp copies `cookies.sqlite` and works
  while Firefox is running; if an older yt-dlp errors, surface the error in the
  log (and document `--cookies-from-browser none` as the escape hatch).
- **Queue identity is server-owned, not heuristic.** Unlike a batch approach
  that leans on yt-dlp's `Downloading video N of M` / `Destination:` strings,
  we assign queue ids at enqueue time and track per-item state ourselves; we
  only read `Destination:`/progress JSON to enrich the active item. If yt-dlp
  changes those strings, the queue still renders correctly -- only the optional
  filename field degrades.
- **Long-lived SSE + snapshot replay.** Every tab holds an open `/events`
  connection for the page lifetime, and a reconnect must get a consistent
  `snapshot` (queue + ring buffer) under the locks. Keep the ring buffer modest
  (e.g. 1000 lines) and the queue cap (Sec. 8) bounded so snapshot payloads
  stay small. `broadcast` lag: a slow tab that falls behind the channel's
  capacity will get a `Lagged` error -- on reconnect it re-snapshots, so this
  is self-healing.
- **Cancel / abort.** Implemented in v1 (see Sec. 6 `/cancel/:id` and Sec. 8).
  The active item carries a `CancellationToken`; POST /cancel trips it and the
  worker `select!` branch `child.kill()`s the process. Pending items are removed
  straight from the queue without involving the worker. `[ retry ]` re-enqueues
  a cancelled/failed item at the back; `[ clear ]` drops terminal items.
- **Persistence robustness.** Writes are atomic (temp + fsync + rename), so a
  crash mid-write cannot corrupt the file -- the worst case is losing the last
  structural mutation (the previous good file remains). The `"version"` field
  lets a future schema migration detect and migrate older files; an unknown
  higher version on load is rejected with a `warn!` (and the file moved aside
  as for a parse error) rather than silently downgraded. The one gap is that the
  file and the download directory can drift if a `done` item's file is deleted
  out-of-band -- persistence tracks *what was queued*, not *what exists on disk*;
  re-running a `done` item is a `[ retry ]` (re-download), not a verify.
- **Library vs. queue drift.** The library scans the directory; the queue
  tracks jobs. They are intentionally decoupled, so a `done` queue item whose
  file was deleted (or a file dropped in by hand) shows up inconsistently
  between the two views. This is accepted: the library is the source of truth
  for *what's on disk*, the queue for *what happened*. A `[ retry ]` on a `done`
  item re-downloads (overwriting); it is not a "the file is missing" detector.
- **Library scan cost.** A non-recursive `read_dir` of `~/Downloads` per
  `library` event + on every SSE connect. For a personal download dir this is
  cheap (tens of files), but a directory with thousands of unrelated files would
  bloat the snapshot payload and the `#library` fragment. Mitigation: cap the
  list (e.g. 200 most-recent by mtime) and note truncation, or scope the
  download dir to a dedicated subdirectory rather than a shared `~/Downloads`.
  If the operator points `--download-dir` at a giant dir, that's their choice.
- **Large file streaming over loopback/LAN.** `/file/:name` must stream
  (`tokio::fs` + chunked/range response), never `read_to_end` -- a multi-GiB
  video buffered into memory would OOM the server and stall every tab. Range
  requests matter for mobile browsers that seek within a video before
  committing to a full download.
- **`confirm()` is the only delete guard.** There is no undo/trash; a misclick
  + confirm deletes a real file. Acceptable for single-user loopback, but the
  confirm dialog must name the file explicitly so the tap is deliberate on
  mobile.
