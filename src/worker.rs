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

    // Sidecar for yt-dlp's `--print-to-file after_move:…`: the authoritative
    // final filename, written once on success. Clear any stale file first --
    // yt-dlp opens it in append mode, so a leftover from a prior attempt of
    // the same item id (e.g. a retry after failure) would otherwise append a
    // second line and our single-line read would return the stale name.
    let sidefile = ytdlp::after_move_sidefile(item_id);
    let _ = tokio::fs::remove_file(&sidefile).await;

    // Build + spawn.
    let mut child = ytdlp::build(&yt_dlp, browser.as_deref(), &download_dir, &sidefile, &url);
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

    // Emit a per-card swap (item just went active) + count + initial status.
    // Targeted, not a full-grid `queue` swap: re-rendering every card here
    // would re-trigger the `.card-overlay` opacity fade-in on the active card
    // and drop `:hover` state on any card the user is interacting with.
    {
        let q = state.queue.lock().await;
        let card = q.get(item_id).map(render::render_card);
        let total = q.items.len();
        let active = q.get(item_id).cloned();
        let pending = q
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Pending)
            .count();
        drop(q);
        if let Some(html) = card {
            state.emit(Event::Card { id: item_id, html });
        }
        state.emit(Event::CardsCount(render::render_cards_count(
            total, pending,
        )));
        // Structural transition (queued/idle -> active): emit every banner
        // slot, since the active item, its thumbnail, title, and cancel
        // button all just changed. This is the only path that touches the
        // thumbnail slot, so the <img> is never recreated on the progress-tick
        // hot path below (which swaps only the bar + meta slots).
        for ev in render::status_events(active.as_ref(), pending) {
            state.emit(ev);
        }
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
    // Separate throttle for the banner's last-log-line subtitle: progress
    // ticks drive the bar/meta slots, log lines drive the log slot, and
    // throttling them independently keeps a chatty yt-dlp from suppressing
    // bar updates (or vice-versa) when one fires right after the other.
    let mut last_log_emit = std::time::Instant::now() - STATUS_THROTTLE;

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
                let _ = tokio::fs::remove_file(&sidefile).await;
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
                let _ = tokio::fs::remove_file(&sidefile).await;
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
                        handle_line(state, item_id, &line, &mut last_status_emit, &mut last_log_emit).await;
                    }
                }
            }
        }
    }

    // Drain any remaining buffered output before waiting on exit.
    while let Some(line) = rx.recv().await {
        handle_line(
            state,
            item_id,
            &line,
            &mut last_status_emit,
            &mut last_log_emit,
        )
        .await;
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
            let _ = tokio::fs::remove_file(&sidefile).await;
            emit_final(state, Some(item_id)).await;
            state.persist().await;
            return;
        }
    };

    // Determine Done / Failed. Media metadata (ffprobe) + native thumbnails
    // are filled by `import::reconcile`, which sweeps the download dir after
    // every completion (and on startup): it probes the just-finished file for
    // codec/duration/resolution and extracts frame_count(duration) frames. Spawned in
    // the background so the worker can immediately proceed to the next item.
    let success = status.success();
    {
        let mut q = state.queue.lock().await;
        if let Some(item) = q.get_mut(item_id) {
            item.cancel = None;
            if success {
                item.status = ItemStatus::Done;
                item.progress = None;
                // Authoritative final filename from yt-dlp's `after_move`
                // sidecar. Fires once, post-merge/move, for every download
                // shape -- including the merge case (where the last progress
                // tick named an intermediate `.fNNN.*` stream) and the
                // already-downloaded case (where yt-dlp emitted no progress
                // JSON at all). Overrides whatever was captured live.
                if let Some(f) = read_after_move_filename(&sidefile).await {
                    item.filename = Some(f);
                }
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
    emit_final(state, Some(item_id)).await;
    state.persist().await;

    // Sweep the download dir: probe the just-finished file's media + generate
    // its native thumbnails (and pick up any other unreferenced files). Only
    // for a successful download; a failure produced no file to import.
    if success {
        tokio::spawn(crate::import::reconcile(state.clone()));
    }
    // Best-effort sidecar cleanup for the failure path (on success the file
    // was already removed by `read_after_move_filename`).
    let _ = tokio::fs::remove_file(&sidefile).await;
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

/// Discard any output lines still buffered in the channel after a cancel so
/// the child's line pumps drain and exit cleanly. The lines are intentionally
/// dropped, not logged: they were produced after the user already asked to
/// stop, so surfacing them would only clutter the log with trailing output
/// from a download that was aborted on purpose.
async fn drain(rx: &mut mpsc::Receiver<String>) {
    while rx.try_recv().is_ok() {}
}

/// Read yt-dlp's `--print-to-file after_move:…` sidecar and return the final
/// on-disk filename's **basename**. The file holds one JSON line shaped
/// `{"status":"after_move","filename":"<abs path>"}`. Returns `None` if the
/// file is absent (yt-dlp failed before `after_move` fired -- e.g. an
/// extraction error) or malformed; non-fatal. Removes the file regardless so
/// nothing accumulates across retries of the same item id.
///
/// This supersedes the old `[Merger] Merging formats into "<path>"` and
/// `[download] <path> has already been downloaded` text scrapes: it is the
/// authoritative final name for *every* download shape, including the merge
/// case (where progress-tick `filename` only ever names an intermediate
/// `.fNNN.*` stream) and the already-downloaded case (where yt-dlp emits zero
/// progress JSON).
async fn read_after_move_filename(sidefile: &std::path::Path) -> Option<String> {
    let text = tokio::fs::read_to_string(sidefile).await.ok()?;
    let _ = tokio::fs::remove_file(sidefile).await;
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let path = value.get("filename")?.as_str()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// Handle one parsed output line: update active item + emit events.
///
/// `last_status_emit` throttles the progress-bar / progress-text slots
/// (`status-bar` / `status-meta`); `last_log_emit` independently throttles the
/// last-log-line slot (`status-log`). Splitting them keeps a chatty yt-dlp
/// from suppressing bar updates (and lets the log subtitle update even when
/// no progress tick has fired recently).
async fn handle_line(
    state: &Arc<AppState>,
    item_id: u64,
    line: &str,
    last_status_emit: &mut std::time::Instant,
    last_log_emit: &mut std::time::Instant,
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
                        // yt-dlp's progress `status:"error"` JSON carries no
                        // human message (the text arrives separately on stderr
                        // as `ERROR:`); record a generic placeholder so the row
                        // surfaces *something* even if no `ERROR:` line
                        // follows this tick.
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

            // Throttled emit of the banner's progress slots only. Always
            // emit on finished/error (the terminal tick that lands 100% /
            // the error state before `emit_final` clears the bar).
            //
            // Only the bar + meta slots update on every throttle tick -- they
            // carry the live progress bar / percent / ETA. The thumbnail,
            // title, and cancel button are untouched here (they change only on
            // structural transitions via `emit_final` / the spawn block), so
            // the `<img>` is not recreated several times a second (the flash
            // the old whole-`#status` outerHTML swap caused). The cards have no
            // progress bar either, so there is no reason to re-render the
            // `#cards` list each tick: doing so destroys and recreates every
            // card's DOM every 200ms, re-triggering the `.card-overlay`
            // opacity fade-in on the active card and dropping `:hover` state
            // on any card the user is interacting with.
            let is_terminal = matches!(status_str.as_deref(), Some("finished") | Some("error"));
            let now = std::time::Instant::now();
            if is_terminal || now.duration_since(*last_status_emit) >= STATUS_THROTTLE {
                *last_status_emit = now;
                need_status = true;
            }

            if need_queue {
                let q = state.queue.lock().await;
                if let Some(html) = q.get(item_id).map(render::render_card) {
                    state.emit(Event::Card { id: item_id, html });
                }
            }
            if need_status {
                let q = state.queue.lock().await;
                let active = q.get(item_id).cloned();
                drop(q);
                // Hot path: only the bar + meta fragments. pending is unused
                // for these two slots (it only feeds the title's queued
                // badge), so pass 0 instead of counting Pending items under
                // the lock on every tick.
                let p = render::StatusParts::new(active.as_ref(), 0);
                state.emit(Event::StatusBar(p.bar));
                state.emit(Event::StatusMeta(p.meta));
            }
        }
        ParsedLine::Log(text) => {
            // Capture the real yt-dlp error message into the active item so the
            // queue row shows "Video unavailable" instead of a generic
            // "yt-dlp exited with status 1". yt-dlp emits progress JSON with
            // `status: "error"` only for mid-download aborts; extraction /
            // format failures print `ERROR: ...` to stderr and exit non-zero,
            // so we must harvest the line here. Last ERROR wins (most specific).
            //
            // This is the only text-line scrape that remains: there is no
            // structured equivalent for extraction/format failures (yt-dlp
            // prints `ERROR:` to stderr and exits non-zero; `after_move` does
            // not fire on failure). The filename is now taken authoritatively
            // from the `--print-to-file after_move:…` sidecar in
            // `run_download`, so the old `[Merger] Merging formats into` and
            // `[download] …has already been downloaded` / `Destination:` text
            // scrapes are gone.
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
                        if let Some(html) = q.get(item_id).map(render::render_card) {
                            state.emit(Event::Card { id: item_id, html });
                        }
                    }
                }
            }

            // Push into the global log ring replayed to newly-connected tabs
            // (not the per-item log -- that is the next block).
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

            // Live-update the banner's latest-log-line slot only. yt-dlp can
            // be chatty, so throttle it (independently from the progress-bar
            // throttle above) -- and swap only the `status-log` slot, never
            // the thumbnail (the flash the old whole-`#status` swap caused).
            let now = std::time::Instant::now();
            if now.duration_since(*last_log_emit) >= STATUS_THROTTLE {
                *last_log_emit = now;
                let q = state.queue.lock().await;
                let active = q.get(item_id).cloned();
                drop(q);
                state.emit(Event::StatusLog(
                    render::StatusParts::new(active.as_ref(), 0).log,
                ));
            }
        }
    }
}

/// Emit the final per-card swap + `cards-count` + the six banner slots for
/// an item transition (or the idle banner when nothing is active). If
/// `active_id` is given, that item's card is re-rendered at its new
/// (terminal / re-queued) status; pass None when the item never entered the
/// queue (spawn failure).
///
/// Structural transition: every banner slot is emitted (the active item, its
/// thumbnail, title, and cancel button may all have changed, or the banner
/// may be transitioning to the queued / idle state). This is the counterpart
/// to the targeted hot path in `handle_line`, which swaps only the bar + meta
/// (+ log) slots.
///
/// Targeted card swap instead of a full-grid `queue` swap: a terminal
/// transition only changes one card, so re-rendering every card would
/// needlessly re-trigger the `.card-overlay` opacity fade-in and drop `:hover`
/// state on any card the user is interacting with. (The full `queue` snapshot
/// is reserved for SSE connect + lag-recovery -- see `Event::Queue`.)
async fn emit_final(state: &Arc<AppState>, active_id: Option<u64>) {
    let (card_html, count_frag, status_evts) = {
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
        let total = q.items.len();
        let card_html = active_id.and_then(|id| q.get(id).map(render::render_card));
        (
            card_html,
            render::render_cards_count(total, pending),
            render::status_events(active.as_ref(), pending),
        )
    };
    if let (Some(id), Some(html)) = (active_id, card_html) {
        state.emit(Event::Card { id, html });
    }
    state.emit(Event::CardsCount(count_frag));
    for ev in status_evts {
        state.emit(ev);
    }
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

    /// A well-formed `after_move` sidecar yields the bare basename of the
    /// final on-disk path.
    #[tokio::test]
    async fn read_after_move_filename_parses_basename() {
        let dir = std::env::temp_dir();
        let path = dir.join("yt-dl-webui-test-after-move-ok.json");
        std::fs::write(
            &path,
            r#"{"status":"after_move","filename":"/tmp/dl/Big Buck Bunny [aqz-KE-bpKQ].mp4"}
"#,
        )
        .unwrap();
        assert_eq!(
            read_after_move_filename(&path).await,
            Some("Big Buck Bunny [aqz-KE-bpKQ].mp4".to_string())
        );
        // File is removed after a successful read.
        assert!(!path.exists());
    }

    /// A missing sidecar (yt-dlp failed before `after_move` fired) yields
    /// `None` -- non-fatal; the item keeps whatever filename the live
    /// progress ticks captured (or `None`).
    #[tokio::test]
    async fn read_after_move_filename_missing_is_none() {
        let path = std::env::temp_dir().join("yt-dl-webui-test-after-move-absent.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_after_move_filename(&path).await, None);
    }

    /// A malformed sidecar yields `None` but is still removed so a corrupt
    /// leftover can't poison the next attempt of the same item id.
    #[tokio::test]
    async fn read_after_move_filename_malformed_is_none_but_removed() {
        let path = std::env::temp_dir().join("yt-dl-webui-test-after-move-bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_after_move_filename(&path).await, None);
        assert!(!path.exists());
    }

    /// A sidecar missing the `filename` key yields `None`.
    #[tokio::test]
    async fn read_after_move_filename_missing_key_is_none() {
        let path = std::env::temp_dir().join("yt-dl-webui-test-after-move-nokey.json");
        std::fs::write(&path, r#"{"status":"after_move"}"#).unwrap();
        assert_eq!(read_after_move_filename(&path).await, None);
        assert!(!path.exists());
    }
}
