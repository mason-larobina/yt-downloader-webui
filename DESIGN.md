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
  a global in-memory queue**. A single background worker drains the queue
  **one URL at a time**, spawning one `yt-dlp` process per item. There is only
  ever one yt-dlp process running at any moment; submitting while a download is
  in flight never rejects -- it just enqueues.
- **Queue and live status are global and shared across all sessions/tabs.**
  Every connected browser sees the same queue and the same active-item
  progress. Still single-user, no auth (see Sec. 9).
- Live, readable progress rendered from yt-dlp's structured progress output
  -- not the raw `\r`-redrawn shell status line.
- Fresh cookies pulled from the home Firefox profile by default
  (`--cookies-from-browser firefox`).
- Configurable download directory via a flag, defaulting to `~/Downloads`.

**Non-goals**
- Multi-user / auth / remote access. This is a local single-user tool bound to
  `127.0.0.1`. (See Sec. 9 Security.) Multi-*session*/multi-tab is supported and
  is a first-class goal above; multi-*user* is not.
- A media library, post-download management UI, or **queue persistence across
  restarts** -- the queue lives in memory; a restart drops pending/active items.
- Supporting browsers/cookie stores other than Firefox *by default* (a flag
  can override, but Firefox is the blessed path).

---

## 2. Stack

| Layer        | Choice                                              |
|--------------|-----------------------------------------------------|
| Language     | Rust (edition 2024) -- per existing `Cargo.toml`    |
| Runtime      | `tokio` (multi-thread)                              |
| HTTP server  | `axum` (good SSE support, pairs well with htmx)     |
| CLI parsing  | `clap` (derive)                                     |
| JSON         | `serde_json`                                        |
| Errors       | `anyhow` (app) + `thiserror` (library-ish types)    |
| Templates    | none needed -- the page is one static `index.html`   |
| Static embed | `include_str!` / `include_bytes!` of `static/`    |
| Frontend     | htmx core + `htmx-ext-sse` (vendored, embedded)     |

The index page is small and fixed, so a template engine is overkill. We embed
`index.html`, `htmx.min.js`, `htmx-ext-sse.js`, and a small `app.css` directly
into the binary with `include_bytes!`/`include_str!` and serve them from `/static/*`
and `/`.

---

## 3. CLI

```
web-dl [OPTIONS]

  -d, --download-dir <DIR>     Where yt-dlp writes files (passed as -P).
                               Default: ~/Downloads
  -b, --cookies-browser <B>    Browser to pull cookies from via
                               --cookies-from-browser. Default: firefox.
                               Use "none" to disable cookies entirely.
      --yt-dlp <PATH>          Path to yt-dlp binary. Default: yt-dlp (PATH).
      --addr <ADDR>            Listen address. Default: 127.0.0.1:8080.
      --bind-all               Bind 0.0.0.0 instead of loopback. DANGEROUS;
                               prints a warning. See Sec. 9.
  -v, --verbose                Verbose server logs.
```

At startup the app runs `yt-dlp --version`; if it is missing or fails it exits
with a clear, actionable message (install yt-dlp / fix `--yt-dlp`).

`~/Downloads` is resolved via the `HOME` env var (falling back to the process's
home as reported by the `home`/`dirs` crate). The resolved path is created if
missing.

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
  leaving yt-dlp's default filename template intact.

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
GET  /            -> index.html (static: textarea form + live output pane)
GET  /static/*    -> embedded htmx.js, sse ext, css
POST /download    -> append URLs to global queue; return ack fragment
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
</div>
```

### GET /events (SSE)
- Upgrades to a long-lived SSE response (`axum::response::sse::Sse` over a
  stream), one per connected tab. The connection is **global and app-lifetime**:
  not tied to any one item, and not closed when an item finishes.
- On connect, emit a `snapshot` event carrying the full current state -- the
  entire queue (pending + active + recent done/failed; see Sec. 8) and the ring
  buffer of recent log lines -- so a freshly opened or reconnected tab is
  immediately consistent with every other tab.
- Thereafter emit named events whose payloads are **HTML fragments**:
  - `status` -> a fresh `<div id="status">...progress bar...</div>` (replaces)
    for the active item; an idle "queue empty / waiting" status when the worker
    parks.
  - `log` -> a `<div class="logline">...escaped text...</div>` (appended).
  - `queue` -> the full queue list `<div id="queue">...</div>` (replaces),
    emitted on queue changes (items enqueued, item starts, item finishes).
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
|   ~ video2.webm (downloading)                 |
|   . video3                                    |
|                                               |
| log:                                          |  <- #log (scrollback)
| [youtube] Extracting URL: ...                   |
| [download] Destination: video1.webm          |
| WARNING: ...                                    |
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
- A second tab opened against the same server renders the identical queue and
  active status (via the `snapshot` event on SSE connect); both tabs stay in
  lockstep via the same global event stream.

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
    status: ItemStatus,       // Pending | Active | Done | Failed
    filename: Option<String>,
    progress: Option<Progress>, // percent/speed/eta/bytes from progress JSON
    error: Option<String>,
    enqueued_at: Instant,
}

enum ItemStatus { Pending, Active, Done, Failed }
```

The `broadcast::Sender<Event>` is created once at startup and lives for the
whole app -- it is **not** per-job. Every `/events` client holds a
`broadcast::Receiver`; multiple tabs all receive the same events. `broadcast`
(not `mpsc`) so a tab reconnecting mid-download still gets subsequent events,
and the `snapshot` event on connect (built from `queue` + `log_ring` under the
locks) makes it consistent with everyone else.

A single worker task is spawned at startup. Its loop:

1. Lock the queue; if there is a pending item, flip it to `Active` and take it;
   otherwise drop the lock and `notify.notified().await` until POST /download
   wakes it.
2. Spawn `yt-dlp` for that one URL. Read stdout/stderr line-by-line, classify
   (Sec. 5), update the active `QueueItem` under the lock, push log lines into
   `log_ring`, and `events.send(...)` the throttled `status`/`log`/`queue`
   fragments.
3. On process exit, mark the item `Done`/`Failed`, emit a final `queue` (and
   final `status`) event, then loop to step 1.

Because only the worker ever spawns yt-dlp, there is only ever one process.
Concurrent POST /download handlers only mutate the queue and `notify`.

Done/failed items are kept in `items` (capped -- e.g. last 200) so the queue
view shows recent history; pending + active are always kept. A v1.1 nice-to-have
is explicit remove/clear from the UI.

---

## 9. Security

- **Bind loopback only by default.** `--bind-all` exists but prints a loud
  warning. Anyone who can reach the server can run `yt-dlp` against arbitrary
  URLs (limited to what `--cookies-from-browser firefox` allows) and read live
  download status -- i.e. effectively act as your Firefox session for these
  sites. Do not expose to a network.
- **No shell.** URLs are `Command::arg`s, never concatenated into a shell
  string -> no command injection.
- **HTML-escape every fragment** sent over SSE; log lines are untrusted text.
- **No arbitrary file traversal**: `--download-dir` is a CLI flag set by the
  operator, not the browser. The browser only chooses URLs.
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

- **`--progress-template '%(progress)j'` exact semantics.** The `%(progress)j`
  outtmpl is documented and widely used, but we should verify on the target
  yt-dlp version that it (a) emits one JSON object per tick and (b) respects
  `--newline` for newline-termination rather than `\r`. If `\r` is still used,
  fall back to byte-level `\r`-aware splitting in `parse.rs` (cheap to add).
  Mitigation: an integration test that runs yt-dlp against a tiny sample URL
  and asserts we can parse the stream end-to-end.
- **SSE swap churn.** If htmx re-rendering the `#status` fragment ~5x/s is
  visually janky, swap to a minimal `EventSource` + `textContent` update for
  `#status`/`#queue` only, keeping htmx for the form and `#log` appends. This
  is a localized change in `index.html` + a few lines of JS, not an
  architectural shift.
- **Firefox profile lock.** Modern yt-dlp copies `cookies.sqlite` and works
  while Firefox is running; if an older yt-dlp errors, surface the error in the
  log (and document `--cookies-browser none` as the escape hatch).
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
- **Cancel / abort.** Not in v1. Easy to add: store the active child
  `Pid`/`Child` in the `QueueItem` (or worker state) and a `POST /cancel` that
  sends `SIGTERM`; also drop the cancelled item (and any pending items if asked)
  from the queue.
```
