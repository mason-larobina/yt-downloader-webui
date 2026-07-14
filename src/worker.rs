//! The single background worker: drains the global queue one URL at a time,
//! spawning one `yt-dlp` process per item.
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::Event;
use crate::parse::{FlatEntry, ParsedLine, parse_flat_line, parse_line};
use crate::render;
use crate::state::{AppState, ItemStatus};
use crate::ytdlp;

/// Throttle: emit status at most every this long, except always emit final.
const STATUS_THROTTLE: Duration = Duration::from_millis(200);

/// Spawn the worker task. Returns its `JoinHandle` so the caller can await
/// clean shutdown (the worker flips any active item to `Pending` and persists
/// before exiting -- see ARCHITECTURE Sec. 8).
pub fn spawn(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(state))
}

async fn run(state: Arc<AppState>) {
    loop {
        if state.shutdown.is_cancelled() {
            break;
        }

        // 1. Take a pending item.
        let taken = {
            let mut q = state.queue.lock().await;
            take_next_pending(&mut q)
        };

        let item = match taken {
            Some(item) => item,
            None => {
                // Park until POST /download enqueues something (or shutdown).
                tokio::select! {
                    _ = state.notify.notified() => {}
                    _ = state.shutdown.cancelled() => break,
                }
                continue;
            }
        };

        // 2. Download this item (the probe is now done synchronously in the
        //    POST /download handler -- the worker only ever downloads).
        run_download(&state, item).await;

        // 3. Sleep between consecutive downloads to rate-limit the source,
        //    but only if another item is queued (no point sleeping when the
        //    queue just emptied). Interruptible by shutdown / a new enqueue
        //    so the wait is never needlessly blocking.
        if state.cfg.sleep > Duration::ZERO && has_pending(&state).await {
            tracing::info!(
                "sleeping {}s before next download",
                state.cfg.sleep.as_secs()
            );
            tokio::select! {
                _ = tokio::time::sleep(state.cfg.sleep) => {}
                _ = state.shutdown.cancelled() => break,
                _ = state.notify.notified() => {}
            }
        }
    }
    tracing::info!("worker exiting");
}

/// Whether the queue currently has any Pending items (used to decide if the
/// inter-download sleep is worth doing).
async fn has_pending(state: &Arc<AppState>) -> bool {
    let q = state.queue.lock().await;
    q.items.iter().any(|i| i.status == ItemStatus::Pending)
}

/// Find the first Pending item, flip it to Active, mint a cancel token, and
/// return a snapshot of the item (id, url). Returns None if nothing pending.
fn take_next_pending(queue: &mut crate::state::Queue) -> Option<u64> {
    let id = queue
        .items
        .iter()
        .find(|i| i.status == ItemStatus::Pending)
        .map(|i| i.id)?;
    if let Some(item) = queue.get_mut(id) {
        item.status = ItemStatus::Active;
        item.progress = None;
        item.error = None;
        item.cancel = Some(CancellationToken::new());
    }
    Some(id)
}

/// The download phase: spawn `yt-dlp` (with `--progress-template`) for one
/// known-single-video URL, parse progress, and mark done/failed. Used both
/// for `Video` items and for a single video that the probe classified.
async fn run_download(state: &Arc<AppState>, item_id: u64) {
    // Snapshot what we need to spawn (under lock), keep the cancel token.
    let (url, browser, download_dir, yt_dlp, cancel) = {
        let q = state.queue.lock().await;
        let item = match q.get(item_id) {
            Some(i) => i,
            None => return,
        };
        let url = item.url.clone();
        let cancel = item.cancel.clone().unwrap_or_else(CancellationToken::new);
        (
            url,
            state.cfg.cookies_from_browser.clone(),
            state.cfg.download_dir.clone(),
            state.cfg.yt_dlp.clone(),
            cancel,
        )
    };

    // Build + spawn.
    let mut child = ytdlp::build(&yt_dlp, browser.as_deref(), &download_dir, &url);
    let child_result = child.spawn();
    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to spawn yt-dlp: {e}");
            tracing::error!("{msg}");
            let mut q = state.queue.lock().await;
            if let Some(item) = q.get_mut(item_id) {
                item.status = ItemStatus::Failed;
                item.error = Some(msg.clone());
                item.cancel = None;
            }
            drop(q);
            emit_final(state, None).await;
            state.persist().await;
            return;
        }
    };

    // Emit a queue swap (item just went active) + initial status.
    {
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
        let active = q.get(item_id).cloned();
        let pending = q
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Pending)
            .count();
        drop(q);
        state.emit(Event::Status(render::render_status(
            active.as_ref(),
            pending,
        )));
    }

    // Wire stdout + stderr into a single mpsc of lines.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, mut rx) = mpsc::channel::<String>(256);

    if let Some(out) = stdout {
        let tx = tx.clone();
        tokio::spawn(async move { pump_lines(out, tx).await });
    }
    if let Some(err) = stderr {
        let tx = tx.clone();
        tokio::spawn(async move { pump_lines(err, tx).await });
    }
    drop(tx); // rx returns None once all senders drop (both pumps done)

    let mut last_status_emit = std::time::Instant::now() - STATUS_THROTTLE;

    loop {
        // Drive output, cancel, and shutdown in a select!.
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                tracing::info!("shutdown: killing yt-dlp for item {item_id}");
                let _ = child.kill().await;
                // Re-queue as Pending so it restarts on next launch.
                {
                    let mut q = state.queue.lock().await;
                    if let Some(item) = q.get_mut(item_id) {
                        item.status = ItemStatus::Pending;
                        item.progress = None;
                        item.cancel = None;
                    }
                }
                emit_final(state, Some(item_id)).await;
                state.persist().await;
                return;
            }
            _ = cancel.cancelled() => {
                tracing::info!("cancel: killing yt-dlp for item {item_id}");
                let _ = child.kill().await;
                drain(&mut rx).await; // best-effort
                {
                    let mut q = state.queue.lock().await;
                    if let Some(item) = q.get_mut(item_id) {
                        item.status = ItemStatus::Cancelled;
                        item.progress = None;
                        item.cancel = None;
                    }
                }
                emit_final(state, Some(item_id)).await;
                state.persist().await;
                return;
            }
            line = rx.recv() => {
                match line {
                    None => {
                        // Both pumps finished -> process is done producing.
                        break;
                    }
                    Some(line) => {
                        handle_line(state, item_id, &line, &mut last_status_emit).await;
                    }
                }
            }
        }
    }

    // Drain any remaining buffered output before waiting on exit.
    while let Some(line) = rx.recv().await {
        handle_line(state, item_id, &line, &mut last_status_emit).await;
    }

    // Wait for the process to exit.
    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("waiting on yt-dlp: {e}");
            // Treat as failed.
            let mut q = state.queue.lock().await;
            if let Some(item) = q.get_mut(item_id) {
                item.status = ItemStatus::Failed;
                item.error = Some(format!("yt-dlp wait error: {e}"));
                item.cancel = None;
            }
            drop(q);
            emit_final(state, Some(item_id)).await;
            state.persist().await;
            return;
        }
    };

    // Determine Done / Failed. Media metadata (ffprobe) + native thumbnails
    // are filled by `import::reconcile`, which sweeps the download dir after
    // every completion (and on startup): it probes the just-finished file for
    // codec/duration/resolution and extracts ln(duration) frames. Spawned in
    // the background so the worker can immediately proceed to the next item.
    let success = status.success();
    {
        let mut q = state.queue.lock().await;
        if let Some(item) = q.get_mut(item_id) {
            item.cancel = None;
            if success {
                item.status = ItemStatus::Done;
                item.progress = None;
                tracing::info!("item {item_id} done");
            } else {
                item.status = ItemStatus::Failed;
                let code = status.code();
                let err = item.error.clone().unwrap_or_else(|| {
                    format!(
                        "yt-dlp exited{}",
                        code.map(|c| format!(" with status {c}"))
                            .unwrap_or_default()
                    )
                });
                item.error = Some(err);
                tracing::warn!("item {item_id} failed");
            }
        }
    }
    // On a finished item, refresh the library so the new file shows up.
    let lib_frag = render::render_library_scan(&state.cfg.download_dir);
    emit_final(state, Some(item_id)).await;
    state.emit(Event::Library(lib_frag));
    state.persist().await;

    // Sweep the download dir: probe the just-finished file's media + generate
    // its native thumbnails (and pick up any other unreferenced files). Only
    // for a successful download; a failure produced no file to import.
    if success {
        tokio::spawn(crate::import::reconcile(state.clone()));
    }
}

/// Read lines from a child pipe and forward them to `tx`.
async fn pump_lines<R: AsyncRead + Unpin + Send + 'static>(pipe: R, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                // yt-dlp with --newline writes '\n'-terminated lines. Strip it.
                let mut line = String::from_utf8_lossy(&buf).into_owned();
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                if tx.send(line).await.is_err() {
                    break; // worker gone
                }
            }
            Err(e) => {
                tracing::debug!("line read error: {e}");
                break;
            }
        }
    }
}

/// Drain any remaining buffered lines (after cancel) into the log.
async fn drain(rx: &mut mpsc::Receiver<String>) {
    while let Ok(line) = rx.try_recv() {
        let _ = line; // discarded for brevity; could log
    }
}

/// Extract the on-disk filename yt-dlp is about to write (or just reported it
/// already wrote) from one of its `[download]` status lines:
///
///   * `[download] Destination: <path>`
///     -- emitted before yt-dlp starts writing a new file.
///   * `[download] <path> has already been downloaded`
///     -- emitted when the file already exists on disk; yt-dlp then exits
///     success **without** emitting any progress JSON or a `Destination:`
///     line, so this is the only place we learn the filename. Without it
///     the Done card would have no `filename` and thus no download/open
///     buttons (and the ffmpeg thumbnail fallback would be skipped).
///
/// Returns the bare basename (last path segment). `None` for any other line.
fn extract_dest_filename(text: &str) -> Option<String> {
    let path = text
        .strip_prefix("[download] Destination:")
        .map(|s| s.trim())
        .or_else(|| {
            let s = text.strip_prefix("[download] ")?;
            let s = s.strip_suffix(" has already been downloaded")?;
            Some(s.trim())
        })?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// Extract the final on-disk filename from yt-dlp's `[Merger]` status line:
///
///   `[Merger] Merging formats into "<path>"`
///
/// yt-dlp emits this *after* it has downloaded every requested stream when a
/// target container (e.g. `-S ext:mp4` / `--merge-output-format mp4`) is in
/// effect. Each stream's `[download] Destination:`/progress tick names only an
/// intermediate per-stream temp file (`.f399.mp4`, `.f141.m4a`, ...), and the
/// last progress tick wins in [`handle_line`], leaving the item labelled with a
/// `.m4a`/`.f141.*` intermediate instead of the real merged output. The
/// `[Merger]` line is the authoritative final name, so it must override any
/// previously captured filename. Returns the bare basename. `None` otherwise.
fn extract_merger_filename(text: &str) -> Option<String> {
    let s = text.strip_prefix("[Merger] Merging formats into")?;
    let s = s.trim();
    // yt-dlp quotes the path: `Merging formats into "/abs/path.mp4"`.
    let s = s.strip_prefix('"')?;
    let s = s.strip_suffix('"')?;
    Some(s.rsplit('/').next().unwrap_or(s).to_string())
}

/// Handle one parsed output line: update active item + emit events.
async fn handle_line(
    state: &Arc<AppState>,
    item_id: u64,
    line: &str,
    last_status_emit: &mut std::time::Instant,
) {
    match parse_line(line) {
        ParsedLine::Progress(prog) => {
            // Capture filename from progress JSON if present.
            let filename = prog.filename.clone().or_else(|| prog.tmpfilename.clone());
            let status_str = prog.status.clone();
            let mut need_status = false;
            let mut need_queue = false;
            {
                let mut q = state.queue.lock().await;
                if let Some(item) = q.get_mut(item_id) {
                    if let Some(f) = filename {
                        // Use basename only for the label. Compare against
                        // the *basename* (not the full path `f`): yt-dlp's
                        // progress JSON reports the full destination path on
                        // every tick, so comparing against `f` would be true
                        // every tick and re-render all cards (via a `queue`
                        // SSE event -> `#cards` innerHTML swap) several times
                        // per second for the whole download. Only re-render
                        // when the label itself actually changes.
                        let base = f.rsplit('/').next().unwrap_or(&f).to_string();
                        if item.filename.as_deref() != Some(&base) {
                            item.filename = Some(base);
                            need_queue = true;
                        }
                    }
                    if status_str.as_deref() == Some("error") {
                        // capture error from progress if any
                        if item.error.is_none() {
                            item.error = Some("yt-dlp reported error".to_string());
                        }
                    }
                    item.progress = Some(prog.clone());
                } else {
                    return;
                }
                drop(q);
            }

            // Throttled status emit. Always emit on finished/error.
            //
            // Only the *banner* (status event) updates on every throttle tick
            // -- it carries the live progress bar / percent / ETA. The cards
            // have no progress bar (progress lives in the banner), so there is
            // no reason to re-render the whole `#cards` list each tick: doing
            // so destroys and recreates every card's DOM every 200ms, which
            // re-triggers the `.card-overlay` opacity fade-in on the active
            // card and drops `:hover` state on any card the user is
            // interacting with. Card-visible changes (filename/label, error,
            // status transitions) are emitted by their own triggers below and
            // by `emit_final` when the item terminates.
            let is_terminal = matches!(status_str.as_deref(), Some("finished") | Some("error"));
            let now = std::time::Instant::now();
            if is_terminal || now.duration_since(*last_status_emit) >= STATUS_THROTTLE {
                *last_status_emit = now;
                need_status = true;
            }

            if need_queue {
                let q = state.queue.lock().await;
                state.emit(Event::Queue(render::render_queue(&q)));
            }
            if need_status {
                let q = state.queue.lock().await;
                let active = q.get(item_id).cloned();
                let pending = q
                    .items
                    .iter()
                    .filter(|i| i.status == ItemStatus::Pending)
                    .count();
                drop(q);
                state.emit(Event::Status(render::render_status(
                    active.as_ref(),
                    pending,
                )));
            }
        }
        ParsedLine::Log(text) => {
            // Capture the real yt-dlp error message into the active item so the
            // queue row shows "Video unavailable" instead of a generic
            // "yt-dlp exited with status 1". yt-dlp emits progress JSON with
            // `status: "error"` only for mid-download aborts; extraction /
            // format failures print `ERROR: ...` to stderr and exit non-zero,
            // so we must harvest the line here. Last ERROR wins (most specific).
            if let Some(rest) = text.strip_prefix("ERROR:") {
                let msg = rest.trim();
                if !msg.is_empty() {
                    let need_queue;
                    {
                        let mut q = state.queue.lock().await;
                        if let Some(item) = q.get_mut(item_id) {
                            // Don't overwrite a more specific mid-download
                            // error already captured from a `status:"error"`
                            // progress tick.
                            if item.error.is_none() {
                                item.error = Some(msg.to_string());
                            }
                        }
                        need_queue = true;
                    }
                    if need_queue {
                        let q = state.queue.lock().await;
                        state.emit(Event::Queue(render::render_queue(&q)));
                    }
                }
            }

            // Try to extract a filename from yt-dlp's status lines (see
            // `extract_dest_filename`). Intermediate per-stream destinations
            // only fill in the name when nothing is set yet.
            let dest_filename = extract_dest_filename(&text);

            if let Some(f) = dest_filename {
                let mut need_queue = false;
                {
                    let mut q = state.queue.lock().await;
                    if let Some(item) = q.get_mut(item_id)
                        && item.filename.is_none()
                    {
                        item.filename = Some(f);
                        need_queue = true;
                    }
                }
                if need_queue {
                    let q = state.queue.lock().await;
                    state.emit(Event::Queue(render::render_queue(&q)));
                }
            }

            // The `[Merger]` line names the final merged file when a target
            // container (e.g. mp4) remuxes separate audio+video streams. It is
            // emitted *after* all per-stream downloads, so it is the
            // authoritative final name and must override the intermediate
            // `.f399.mp4` / `.f141.m4a` filenames captured above.
            if let Some(f) = extract_merger_filename(&text) {
                let mut need_queue = false;
                {
                    let mut q = state.queue.lock().await;
                    if let Some(item) = q.get_mut(item_id)
                        && item.filename.as_deref() != Some(&f)
                    {
                        item.filename = Some(f);
                        need_queue = true;
                    }
                }
                if need_queue {
                    let q = state.queue.lock().await;
                    state.emit(Event::Queue(render::render_queue(&q)));
                }
            }

            // Push into ring buffer and emit a log line.
            {
                let mut ring = state.log_ring.lock().await;
                ring.push(text.clone());
            }
            // Also capture the line on the item itself so the per-video
            // logs pane can show this download's output at any point
            // (in-progress or completed); persisted with the item.
            {
                let mut q = state.queue.lock().await;
                if let Some(item) = q.get_mut(item_id) {
                    item.push_log(text.clone());
                }
            }
            state.emit(Event::Log(render::render_log_line(&text)));

            // Live-update the banner's latest-log-line subtitle. yt-dlp can
            // be chatty, so reuse the same throttle as progress ticks.
            let now = std::time::Instant::now();
            if now.duration_since(*last_status_emit) >= STATUS_THROTTLE {
                *last_status_emit = now;
                let q = state.queue.lock().await;
                let active = q.get(item_id).cloned();
                let pending = q
                    .items
                    .iter()
                    .filter(|i| i.status == ItemStatus::Pending)
                    .count();
                drop(q);
                state.emit(Event::Status(render::render_status(
                    active.as_ref(),
                    pending,
                )));
            }
        }
    }
}

/// Emit the final `status` + `queue` for an item transition (or the idle
/// status when nothing is active). If `active_id` is given, that item is still
/// rendered as the active status before clearing; pass None to render idle.
async fn emit_final(state: &Arc<AppState>, active_id: Option<u64>) {
    let (queue_frag, status_frag) = {
        let q = state.queue.lock().await;
        // Only render the progress bar for an item that is *still* Active.
        // Once the item has transitioned to Done/Failed/Cancelled/Pending the
        // worker is parked (or about to pop the next item), so the status
        // pane should show the idle "queue empty" line -- otherwise a finished
        // item with cleared progress would render a bogus 0% bar.
        let active = active_id
            .and_then(|id| q.get(id))
            .filter(|i| i.status == ItemStatus::Active)
            .cloned();
        let pending = q
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Pending)
            .count();
        (
            render::render_queue(&q),
            render::render_status(active.as_ref(), pending),
        )
    };
    state.emit(Event::Status(status_frag));
    state.emit(Event::Queue(queue_frag));
}

// ---------------------------------------------------------------------------
// Streaming probe (drives the GET /probe SSE stream)
// ---------------------------------------------------------------------------

/// How long a probe is allowed to run before we give up and kill it. The
/// probe is `--flat-playlist -j` (no download), so this is generous; it only
/// guards against a hung yt-dlp hanging the SSE stream.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Outcome of probing one submitted URL with `yt-dlp --flat-playlist -j`.
///
/// The probe classifies the URL without downloading:
/// - a **playlist** yields N `_type:"url"` entries whose `url` is already the
///   full per-video watch URL -> presented as confirm cards (never persisted);
/// - a **single video** yields one full video dict (no playlist entries) ->
///   the caller enqueues the *original* submitted URL directly (the dict's
///   `url` is a media URL, not a watch URL), borrowing only title/duration.
///
/// Because `kill_on_drop(true)` is set on the command, a dropped probe future
/// (client disconnects the SSE stream, server shutdown) kills the child
/// automatically.
pub struct ProbeOutcome {
    /// Playlist entries (`_type:"url"` with a `url`). Empty for a single video.
    pub entries: Vec<FlatEntry>,
    /// A single-video dict, if the URL was not a playlist. Its `url` is the
    /// *media* URL (do not enqueue); borrow `title`/`duration` only.
    pub single: Option<FlatEntry>,
    /// Best-effort error message captured from stderr (`ERROR:`) or a spawn /
    /// timeout failure. `None` when nothing went wrong.
    pub error: Option<String>,
}

impl ProbeOutcome {
    fn error(msg: impl Into<String>) -> Self {
        ProbeOutcome {
            entries: Vec::new(),
            single: None,
            error: Some(msg.into()),
        }
    }
}

/// One event emitted by the streaming probe ([`probe_stream`]).
pub enum ProbeEvent {
    /// One yt-dlp output line (stdout or stderr); forwarded to the SSE
    /// stream's `log` event so the header shows live probe progress.
    Log(String),
    /// The probe finished; carries the classified [`ProbeOutcome`] rendered
    /// as the `result` SSE event (confirm cards or an error + Done button).
    Done(Box<ProbeOutcome>),
}

/// Probe one submitted URL with `--flat-playlist -j`, streaming each yt-dlp
/// output line as a [`ProbeEvent::Log`] and finishing with a single
/// [`ProbeEvent::Done`]. Runs concurrently with the worker's downloads (it
/// writes no files, emits no progress ticks) and bounds itself with
/// [`PROBE_TIMEOUT`]. Stderr `ERROR:` lines are harvested into
/// [`ProbeOutcome::error`]; nothing is broadcast to the global log ring /
/// `/events` SSE (the probe is a private request-handler interaction whose
/// result is returned to the caller's dedicated `/probe` stream).
///
/// The returned stream owns the spawned `yt-dlp` child (`kill_on_drop`), so
/// dropping the stream -- e.g. when the browser closes the EventSource
/// (cancel) or navigates away -- kills the probe process promptly.
pub fn probe_stream(
    state: Arc<AppState>,
    url: String,
) -> impl futures_util::Stream<Item = ProbeEvent> {
    stream! {
        let (yt_dlp, browser) = {
            let cfg = &state.cfg;
            (cfg.yt_dlp.clone(), cfg.cookies_from_browser.clone())
        };

        let mut child = match ytdlp::build_probe(&yt_dlp, browser.as_deref(), &url).spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to spawn yt-dlp probe: {e}");
                yield ProbeEvent::Log(format!("ERROR: {msg}"));
                yield ProbeEvent::Done(Box::new(ProbeOutcome::error(msg)));
                return;
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, mut rx) = mpsc::channel::<String>(256);
        if let Some(out) = stdout {
            let tx = tx.clone();
            tokio::spawn(async move { pump_lines(out, tx).await });
        }
        if let Some(err) = stderr {
            let tx = tx.clone();
            tokio::spawn(async move { pump_lines(err, tx).await });
        }
        drop(tx);

        let mut entries: Vec<FlatEntry> = Vec::new();
        let mut single: Option<FlatEntry> = None;
        let mut err_msg: Option<String> = None;

        // Overall deadline (not per-line): a single pinned sleep future
        // advanced across the whole loop.
        let deadline = tokio::time::sleep(PROBE_TIMEOUT);
        tokio::pin!(deadline);
        let mut timed_out = false;

        // Watch the global shutdown token too: axum's graceful shutdown waits
        // for in-flight response streams to complete, and this loop otherwise
        // only ends on the 60s deadline or yt-dlp EOF. Without this branch a
        // probe running at Ctrl+C time would block server shutdown for up to
        // PROBE_TIMEOUT -- looking like "Ctrl+C doesn't stop the server".
        let shutdown = state.shutdown.cancelled();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    // Server is shutting down: kill the probe child (also
                    // kill_on_drop, but do it explicitly so the exit is
                    // logged as clean) and end the stream without a result --
                    // the client is going away with the server anyway.
                    let _ = child.kill().await;
                    return;
                }
                _ = &mut deadline => {
                    timed_out = true;
                    break;
                }
                line = rx.recv() => {
                    match line {
                        None => break,
                        Some(line) => {
                            yield ProbeEvent::Log(line.clone());
                            if let Some(entry) = parse_flat_line(&line) {
                                if entry.is_playlist_entry() {
                                    entries.push(entry);
                                } else if single.is_none() {
                                    single = Some(entry);
                                }
                                continue;
                            }
                            // Non-JSON line (rare on stdout; common on
                            // stderr). Harvest the last `ERROR:` line as the
                            // surfaced message.
                            if let Some(rest) = line.strip_prefix("ERROR:") {
                                let msg = rest.trim();
                                if !msg.is_empty() {
                                    err_msg = Some(msg.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        if timed_out {
            // Kill the (possibly still running) child and bail.
            let _ = child.kill().await;
            yield ProbeEvent::Done(Box::new(ProbeOutcome::error(format!(
                "probe timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            ))));
            return;
        }

        // The pumps close their senders on EOF, so by here the child has exited.
        let status = match child.wait().await {
            Ok(s) => s,
            Err(e) => {
                yield ProbeEvent::Done(Box::new(ProbeOutcome::error(format!(
                    "yt-dlp probe wait error: {e}"
                ))));
                return;
            }
        };

        // If we got entries or a single video, the probe succeeded for our
        // purposes even if yt-dlp printed a trailing WARNING; otherwise
        // surface the captured error (or a generic exit-status message).
        if !entries.is_empty() || single.is_some() {
            yield ProbeEvent::Done(Box::new(ProbeOutcome {
                entries,
                single,
                error: None,
            }));
            return;
        }
        let success = status.success();
        yield ProbeEvent::Done(Box::new(ProbeOutcome {
            entries,
            single,
            error: err_msg.or_else(|| {
                if success {
                    None
                } else {
                    Some(format!(
                        "yt-dlp probe failed{}",
                        status
                            .code()
                            .map(|c| format!(" (status {c})"))
                            .unwrap_or_default()
                    ))
                }
            }),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[download] Destination: <path>` yields the bare basename.
    #[test]
    fn extract_dest_filename_destination() {
        assert_eq!(
            extract_dest_filename("[download] Destination: /tmp/redl/Video.mp4"),
            Some("Video.mp4".to_string())
        );
        // Leading/trailing whitespace tolerated.
        assert_eq!(
            extract_dest_filename("[download] Destination:   /a/b/Cool Clip.webm  "),
            Some("Cool Clip.webm".to_string())
        );
    }

    /// `[download] <path> has already been downloaded` is the line yt-dlp
    /// prints when re-downloading a URL whose file already exists. It emits
    /// no progress JSON and no `Destination:` line, so this is the only
    /// source of the filename -- without it the Done card has no download /
    /// open buttons. Regression for the reported bug.
    #[test]
    fn extract_dest_filename_already_downloaded() {
        assert_eq!(
            extract_dest_filename(
                "[download] /tmp/redl/Big Buck Bunny [aqz-KE-bpKQ].mp4 has already been downloaded"
            ),
            Some("Big Buck Bunny [aqz-KE-bpKQ].mp4".to_string())
        );
    }

    /// Unrelated log lines yield no filename.
    #[test]
    fn extract_dest_filename_other_lines() {
        assert_eq!(
            extract_dest_filename("[youtube] aqz-KE-bpKQ: Downloading webpage"),
            None
        );
        assert_eq!(
            extract_dest_filename("[info] aqz-KE-bpKQ: Downloading 1 format(s): 399+258"),
            None
        );
        assert_eq!(extract_dest_filename("ERROR: video unavailable"), None);
        assert_eq!(extract_dest_filename(""), None);
    }

    /// `[Merger] Merging formats into "<path>"` yields the bare basename of
    /// the final merged file. Regression for the reported bug where, with a
    /// target mp4 container, the item ended up labelled with the last
    /// intermediate stream's name (`.f141.m4a`) instead of the merged `.mp4`.
    #[test]
    fn extract_merger_filename_basic() {
        assert_eq!(
            extract_merger_filename(
                "[Merger] Merging formats into \"/home/lambo/Downloads/yt-dlp/Awaken from the Dark Slumber (Spring) [jgoOzh_DZuw].mp4\""
            ),
            Some("Awaken from the Dark Slumber (Spring) [jgoOzh_DZuw].mp4".to_string())
        );
    }

    /// Non-merger lines (including the intermediate `[download] Destination`
    /// lines for per-stream temp files) must not match the merger extractor.
    #[test]
    fn extract_merger_filename_other_lines() {
        assert_eq!(
            extract_merger_filename(
                "[download] Destination: /tmp/redl/Awaken from the Dark Slumber (Spring) [jgoOzh_DZuw].f141.m4a"
            ),
            None
        );
        assert_eq!(
            extract_merger_filename(
                "Deleting original file /tmp/redl/x.f141.m4a (pass -k to keep)"
            ),
            None
        );
        assert_eq!(extract_merger_filename(""), None);
    }
}
