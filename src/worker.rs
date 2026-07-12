//! The single background worker: drains the global queue one URL at a time,
//! spawning one `yt-dlp` process per item.
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::Event;
use crate::parse::{ParsedLine, parse_flat_line, parse_line};
use crate::render;
use crate::state::{AppState, ItemKind, ItemStatus, QueueItem};
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

        // 2. Run yt-dlp for this one item.
        run_item(&state, item).await;
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

/// Dispatch a queued item to its phase based on `kind`:
/// - `Probe` (a submitted URL of unknown type) -> classify via
///   `--flat-playlist -j`: a playlist is expanded into per-video `Video`
///   items, a single video falls through to [`run_download`].
/// - `Video` (known single video) -> download directly via [`run_download`].
async fn run_item(state: &Arc<AppState>, item_id: u64) {
    let kind = {
        let q = state.queue.lock().await;
        q.get(item_id).map(|i| i.kind).unwrap_or(ItemKind::Probe)
    };
    match kind {
        ItemKind::Probe => run_probe(state, item_id).await,
        ItemKind::Video => run_download(state, item_id).await,
    }
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

    // Determine Done / Failed. Pull any captured error from the active item.
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

/// The probe phase: classify a submitted URL with `--flat-playlist -j` without
/// downloading. A playlist (`_type:"url"` entries) is expanded into pending
/// per-video `Video` items, streamed live as entries arrive; a single video
/// (a full video dict with no `playlist_index`) borrows its title/duration and
/// falls through to [`run_download`] for the actual download. Nothing is
/// persisted mid-probe -- a crash re-runs the probe from scratch on restart,
/// avoiding duplicated per-video items; the final state is persisted once at
/// the end.
async fn run_probe(state: &Arc<AppState>, item_id: u64) {
    // Snapshot what we need to spawn (under lock), keep the cancel token.
    let (url, browser, yt_dlp, cancel) = {
        let q = state.queue.lock().await;
        let item = match q.get(item_id) {
            Some(i) => i,
            None => return,
        };
        (
            item.url.clone(),
            state.cfg.cookies_from_browser.clone(),
            state.cfg.yt_dlp.clone(),
            item.cancel.clone().unwrap_or_else(CancellationToken::new),
        )
    };

    // Build + spawn the probe (no -P, no progress template -- never writes
    // files or emits progress ticks).
    let mut cmd = ytdlp::build_probe(&yt_dlp, browser.as_deref(), &url);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to spawn yt-dlp probe: {e}");
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
    drop(tx);

    let mut enqueued: u64 = 0;
    let mut playlist_title: Option<String> = None;
    let mut single: Option<(Option<String>, Option<f64>)> = None;

    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                tracing::info!("shutdown: killing yt-dlp probe for item {item_id}");
                let _ = child.kill().await;
                // Re-queue as Pending so it re-probes on next launch.
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
                tracing::info!("cancel: killing yt-dlp probe for item {item_id}");
                let _ = child.kill().await;
                drain(&mut rx).await;
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
            line = rx.recv() => match line {
                None => break,
                Some(line) => {
                    probe_handle_line(
                        state, item_id, &line, &mut enqueued, &mut playlist_title, &mut single,
                    ).await;
                }
            }
        }
    }

    // Drain any remaining buffered output before waiting on exit.
    while let Some(line) = rx.recv().await {
        probe_handle_line(
            state, item_id, &line, &mut enqueued, &mut playlist_title, &mut single,
        ).await;
    }

    let exit_status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("waiting on yt-dlp probe: {e}");
            let mut q = state.queue.lock().await;
            if let Some(item) = q.get_mut(item_id) {
                item.status = ItemStatus::Failed;
                item.error = Some(format!("yt-dlp probe wait error: {e}"));
                item.cancel = None;
            }
            drop(q);
            emit_final(state, Some(item_id)).await;
            state.persist().await;
            return;
        }
    };

    if enqueued > 0 {
        // Playlist expanded into per-video items: mark the probe item done.
        let label = match &playlist_title {
            Some(t) => format!("playlist: {t} ({enqueued} videos)"),
            None => format!("playlist ({enqueued} videos)"),
        };
        {
            let mut q = state.queue.lock().await;
            if let Some(item) = q.get_mut(item_id) {
                item.status = ItemStatus::Done;
                item.title = Some(label.clone());
                item.progress = None;
                item.cancel = None;
            }
        }
        tracing::info!("item {item_id} expanded playlist into {enqueued} video items");
        emit_final(state, Some(item_id)).await;
        state.persist().await;
        return;
    }

    if let Some((title, duration)) = single {
        // Single video: borrow title/duration, then download the original URL
        // via the download phase (same item, same cancel token).
        {
            let mut q = state.queue.lock().await;
            if let Some(item) = q.get_mut(item_id) {
                item.title = title;
                item.duration = duration;
            }
        }
        run_download(state, item_id).await;
        return;
    }

    // Neither entries nor a single video: empty playlist or extraction error.
    let success = exit_status.success();
    let err = {
        let q = state.queue.lock().await;
        q.get(item_id).and_then(|i| i.error.clone())
    };
    {
        let mut q = state.queue.lock().await;
        if let Some(item) = q.get_mut(item_id) {
            item.cancel = None;
            if success {
                item.status = ItemStatus::Done;
                item.title = Some("no videos extracted".to_string());
                item.progress = None;
            } else {
                item.status = ItemStatus::Failed;
                item.error = Some(err.unwrap_or_else(|| {
                    format!(
                        "yt-dlp probe failed{}",
                        exit_status.code().map(|c| format!(" (status {c})")).unwrap_or_default()
                    )
                }));
            }
        }
    }
    emit_final(state, Some(item_id)).await;
    state.persist().await;
}

/// Handle one probe output line: a flat-playlist entry (enqueue a per-video
/// `Video` item, streamed live), a single-video dict (capture title/duration
/// for the fall-through download), or a log line (harvest `ERROR:` into the
/// item's error field and echo it to the log ring).
async fn probe_handle_line(
    state: &Arc<AppState>,
    item_id: u64,
    line: &str,
    enqueued: &mut u64,
    playlist_title: &mut Option<String>,
    single: &mut Option<(Option<String>, Option<f64>)>,
) {
    if let Some(entry) = parse_flat_line(line) {
        if entry.is_playlist_entry() {
            if playlist_title.is_none() {
                *playlist_title = entry.playlist_title.clone();
            }
            let vurl = entry.url.clone().unwrap();
            let vtitle = entry.title.clone();
            let vduration = entry.duration;
            {
                let mut q = state.queue.lock().await;
                q.enqueue_video(vurl, vtitle, vduration);
            }
            *enqueued += 1;
            // Live queue swap so per-video rows stream in as entries arrive.
            {
                let q = state.queue.lock().await;
                state.emit(Event::Queue(render::render_queue(&q)));
            }
        } else if single.is_none() {
            // Single-video dict: `url` here is the *media* URL, so we keep the
            // original submitted URL for download and borrow only title/duration.
            *single = Some((entry.title.clone(), entry.duration));
        }
        return;
    }
    // Log line: WARNING:/ERROR:/status. Harvest ERROR: into item.error.
    if let Some(rest) = line.strip_prefix("ERROR:") {
        let msg = rest.trim();
        if !msg.is_empty() {
            let mut q = state.queue.lock().await;
            if let Some(item) = q.get_mut(item_id) {
                if item.error.is_none() {
                    item.error = Some(msg.to_string());
                }
            }
            drop(q);
            let q = state.queue.lock().await;
            state.emit(Event::Queue(render::render_queue(&q)));
        }
    }
    {
        let mut ring = state.log_ring.lock().await;
        ring.push(line.to_string());
    }
    state.emit(Event::Log(render::render_log_line(line)));
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

#[allow(dead_code)]
fn _unused(_item: &QueueItem) {}
