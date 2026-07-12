//! The single background worker: drains the global queue one URL at a time,
//! spawning one `yt-dlp` process per item.
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
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
/// before exiting -- see DESIGN Sec. 8).
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
    }
    tracing::info!("worker exiting");
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
        let cancel = item
            .cancel
            .clone()
            .unwrap_or_else(CancellationToken::new);
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
        drop(q);
        state.emit(Event::Status(render::render_status(active.as_ref())));
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

    // Determine Done / Failed. For a successful download with no thumbnail
    // yet, generate one from the downloaded file with ffmpeg (best-effort,
    // captured here so the final queue event includes the thumb). Runs without
    // the queue lock held.
    let success = status.success();
    let mut gen_thumb: Option<String> = None;
    if success {
        let (have_thumb, video_path) = {
            let q = state.queue.lock().await;
            let item = match q.get(item_id) {
                Some(i) => i,
                None => return,
            };
            (
                item.thumbnail.clone(),
                item.filename
                    .clone()
                    .map(|base| state.cfg.download_dir.join(base)),
            )
        };
        if have_thumb.is_none() {
            if let Some(vp) = video_path {
                match crate::thumb::generate(&state.cfg.ffmpeg, &state.cfg.cache_dir, &vp).await {
                    Ok(fname) => gen_thumb = Some(fname),
                    Err(e) => {
                        tracing::debug!(
                            "ffmpeg thumbnail generation failed for item {item_id}: {e:#}"
                        );
                    }
                }
            }
        }
    }
    {
        let mut q = state.queue.lock().await;
        if let Some(item) = q.get_mut(item_id) {
            item.cancel = None;
            if success {
                item.status = ItemStatus::Done;
                item.progress = None;
                if let Some(fname) = &gen_thumb {
                    if item.thumbnail.is_none() {
                        item.thumbnail = Some(fname.clone());
                    }
                }
                tracing::info!("item {item_id} done");
            } else {
                item.status = ItemStatus::Failed;
                let code = status.code();
                let err = item.error.clone().unwrap_or_else(|| {
                    format!(
                        "yt-dlp exited{}",
                        code.map(|c| format!(" with status {c}")).unwrap_or_default()
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
}

/// Read lines from a child pipe and forward them to `tx`.
async fn pump_lines<R: AsyncRead + Unpin + Send + 'static>(
    pipe: R,
    tx: mpsc::Sender<String>,
) {
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

use tokio::io::AsyncRead;

/// Drain any remaining buffered lines (after cancel) into the log.
async fn drain(rx: &mut mpsc::Receiver<String>) {
    while let Ok(line) = rx.try_recv() {
        let _ = line; // discarded for brevity; could log
    }
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
            let filename = prog
                .filename
                .clone()
                .or_else(|| prog.tmpfilename.clone());
            let status_str = prog.status.clone();
            let mut need_status = false;
            let mut need_queue = false;
            {
                let mut q = state.queue.lock().await;
                if let Some(item) = q.get_mut(item_id) {
                    if let Some(f) = filename {
                        if item.filename.as_deref() != Some(&f) {
                            // Use basename only for the label.
                            let base = f
                                .rsplit('/')
                                .next()
                                .unwrap_or(&f)
                                .to_string();
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
            let is_terminal = matches!(status_str.as_deref(), Some("finished") | Some("error"));
            let now = std::time::Instant::now();
            if is_terminal || now.duration_since(*last_status_emit) >= STATUS_THROTTLE {
                *last_status_emit = now;
                need_status = true;
                // Re-render the cards on every throttle tick so the active
                // card's progress bar advances (not just on filename change).
                need_queue = true;
            }

            if need_queue {
                let q = state.queue.lock().await;
                state.emit(Event::Queue(render::render_queue(&q)));
            }
            if need_status {
                let q = state.queue.lock().await;
                let active = q.get(item_id).cloned();
                drop(q);
                state.emit(Event::Status(render::render_status(active.as_ref())));
            }
        }
        ParsedLine::FlatEntry(_) => {
            // A flat-playlist entry line is not expected during a download
            // (the download uses --progress-template, not -j); ignore it.
            tracing::debug!("unexpected FlatEntry line in download phase");
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

            // Try to extract a filename from `[download] Destination: <path>`.
            let dest_filename = text
                .strip_prefix("[download] Destination:")
                .map(|s| s.trim())
                .map(|s| s.rsplit('/').next().unwrap_or(s).to_string());

            if let Some(f) = dest_filename {
                let mut need_queue = false;
                {
                    let mut q = state.queue.lock().await;
                    if let Some(item) = q.get_mut(item_id) {
                        if item.filename.is_none() {
                            item.filename = Some(f);
                            need_queue = true;
                        }
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
        (render::render_queue(&q), render::render_status(active.as_ref()))
    };
    state.emit(Event::Status(status_frag));
    state.emit(Event::Queue(queue_frag));
}

// ---------------------------------------------------------------------------
// Synchronous probe (runs in POST /download, not the worker)
// ---------------------------------------------------------------------------

/// How long a probe is allowed to run before we give up and kill it. The
/// probe is `--flat-playlist -j` (no download), so this is generous; it only
/// guards against a hung yt-dlp hanging the request handler.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Outcome of probing one submitted URL with `yt-dlp --flat-playlist -j`.
///
/// The probe classifies the URL without downloading:
/// - a **playlist** yields N `_type:"url"` entries whose `url` is already the
///   full per-video watch URL -> presented for approval (never persisted);
/// - a **single video** yields one full video dict (no playlist entries) ->
///   the caller enqueues the *original* submitted URL directly (the dict's
///   `url` is a media URL, not a watch URL), borrowing only title/duration.
///
/// Because `kill_on_drop(true)` is set on the command, a dropped probe future
/// (request cancelled, server shutdown) kills the child automatically.
pub struct ProbeOutcome {
    /// Playlist entries (`_type:"url"` with a `url`). Empty for a single video.
    pub entries: Vec<FlatEntry>,
    /// A single-video dict, if the URL was not a playlist. Its `url` is the
    /// *media* URL (do not enqueue); borrow `title`/`duration` only.
    pub single: Option<FlatEntry>,
    /// yt-dlp exited successfully (exit code 0).
    pub success: bool,
    /// Best-effort error message captured from stderr (`ERROR:`) or a spawn /
    /// timeout failure. `None` when nothing went wrong.
    pub error: Option<String>,
}

impl ProbeOutcome {
    fn error(msg: impl Into<String>) -> Self {
        ProbeOutcome {
            entries: Vec::new(),
            single: None,
            success: false,
            error: Some(msg.into()),
        }
    }
}

/// Probe one submitted URL with `--flat-playlist -j` and classify it. Runs
/// concurrently with the worker's downloads (it writes no files, emits no
/// progress ticks) and bounds itself with [`PROBE_TIMEOUT`]. Stderr `ERROR:`
/// lines are harvested into [`ProbeOutcome::error`]; nothing is broadcast to
/// the global log ring / SSE (the probe is a private request-handler
/// interaction whose result is returned to the caller).
pub async fn probe(state: &Arc<AppState>, url: &str) -> ProbeOutcome {
    let (yt_dlp, browser) = {
        let cfg = &state.cfg;
        (cfg.yt_dlp.clone(), cfg.cookies_from_browser.clone())
    };

    let mut child = match ytdlp::build_probe(&yt_dlp, browser.as_deref(), url).spawn() {
        Ok(c) => c,
        Err(e) => return ProbeOutcome::error(format!("failed to spawn yt-dlp probe: {e}")),
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

    let drain_result = tokio::time::timeout(PROBE_TIMEOUT, async {
        while let Some(line) = rx.recv().await {
            if let Some(entry) = parse_flat_line(&line) {
                if entry.is_playlist_entry() {
                    entries.push(entry);
                } else if single.is_none() {
                    single = Some(entry);
                }
                continue;
            }
            // Non-JSON line (rare on stdout; common on stderr). Harvest the
            // last `ERROR:` line as the surfaced message.
            if let Some(rest) = line.strip_prefix("ERROR:") {
                let msg = rest.trim();
                if !msg.is_empty() {
                    err_msg = Some(msg.to_string());
                }
            }
        }
    })
    .await;

    if drain_result.is_err() {
        // Timed out: kill the (possibly still running) child and bail.
        let _ = child.kill().await;
        return ProbeOutcome::error(format!(
            "probe timed out after {}s",
            PROBE_TIMEOUT.as_secs()
        ));
    }

    // The pumps close their senders on EOF, so by here the child has exited.
    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => return ProbeOutcome::error(format!("yt-dlp probe wait error: {e}")),
    };

    let success = status.success();
    // If we got entries or a single video, the probe succeeded for our
    // purposes even if yt-dlp printed a trailing WARNING; otherwise surface
    // the captured error (or a generic exit-status message).
    if !entries.is_empty() || single.is_some() {
        return ProbeOutcome {
            entries,
            single,
            success: true,
            error: None,
        };
    }
    ProbeOutcome {
        entries,
        single,
        success,
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
    }
}
