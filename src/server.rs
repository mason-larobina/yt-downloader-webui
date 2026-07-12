//! Axum router, route handlers, and the SSE stream.
use std::sync::Arc;

use async_stream::stream;
use axum::body::{Body, Bytes};
use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::events::Event;
use crate::library;
use crate::parse::FlatEntry;
use crate::render;
use crate::state::{AppState, ItemStatus};
use crate::worker;

/// Build the application router.
pub fn router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(index))
        .route("/static/htmx.min.js", axum::routing::get(static_htmx))
        .route("/static/htmx-ext-sse.js", axum::routing::get(static_sse))
        .route("/static/app.css", axum::routing::get(static_css))
        .route("/download", axum::routing::post(post_download))
        .route("/approve", axum::routing::post(post_approve))
        .route("/cancel/{id}", axum::routing::post(post_cancel))
        .route("/retry/{id}", axum::routing::post(post_retry))
        .route("/clear", axum::routing::post(post_clear))
        .route("/library", axum::routing::get(library::get_library))
        .route("/file/{name}", axum::routing::get(library::get_file))
        .route("/delete/{name}", axum::routing::post(library::delete_file))
        .route("/events", axum::routing::get(get_events))
        .with_state(state)
}

// ------------------------------ static -------------------------------------

const INDEX_HTML: &str = include_str!("../static/index.html");
const HTMX_JS: &[u8] = include_bytes!("../static/htmx.min.js");
const HTMX_SSE_JS: &[u8] = include_bytes!("../static/htmx-ext-sse.js");
const APP_CSS: &str = include_str!("../static/app.css");

async fn index() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    (StatusCode::OK, headers, INDEX_HTML).into_response()
}

async fn static_htmx() -> Response {
    bytes_response(HTMX_JS, "application/javascript; charset=utf-8")
}

async fn static_sse() -> Response {
    bytes_response(HTMX_SSE_JS, "application/javascript; charset=utf-8")
}

async fn static_css() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    (StatusCode::OK, headers, APP_CSS).into_response()
}

fn bytes_response(data: &'static [u8], content_type: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
    (StatusCode::OK, headers, Body::from(data)).into_response()
}

// ------------------------------ /download ----------------------------------

#[derive(Deserialize)]
pub struct DownloadForm {
    pub urls: String,
}

/// One decoded approval checkbox value (see `render::render_approval`). The
/// checkbox `value` is the JSON serialisation of this; POST /approve gets the
/// browser-decoded JSON strings back as repeated `entry` form fields.
#[derive(Deserialize)]
struct ApproveEntry {
    url: String,
    title: Option<String>,
    duration: Option<f64>,
}

/// POST /download -- probe each pasted URL synchronously (fast: `--flat-playlist
/// -j`, no download), then either enqueue single videos directly or return a
/// playlist's entries as an approval list. The probe runs concurrently with
/// the worker's downloads; only per-video items are ever persisted.
///
/// Returns a fragment swapped into `#approve`:
/// - single video(s) -> `<span class="ack">queued N download(s)</span>` (the
///   items are already in the queue);
/// - playlist -> `render::render_approval(...)` (a form of checkboxes);
/// - mixed -> approval list with a note naming the directly-queued count and
///   any per-URL probe errors;
/// - all failed / empty -> `<span class="err">...</span>`.
async fn post_download(
    State(state): State<Arc<AppState>>,
    Form(form): Form<DownloadForm>,
) -> String {
    let urls: Vec<String> = form
        .urls
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if urls.is_empty() {
        return r#"<span class="err">paste at least one URL</span>"#.to_string();
    }

    let mut direct: u64 = 0;
    let mut entries: Vec<FlatEntry> = Vec::new();
    let mut playlist_title: Option<String> = None;
    let mut errors: Vec<(String, String)> = Vec::new();

    for u in &urls {
        let outcome = worker::probe(&state, u).await;
        if !outcome.entries.is_empty() {
            if playlist_title.is_none() {
                playlist_title = outcome
                    .entries
                    .first()
                    .and_then(|e| e.playlist_title.clone());
            }
            entries.extend(outcome.entries);
        } else if let Some(sv) = outcome.single {
            let title = sv.title.clone();
            let duration = sv.duration;
            {
                let mut q = state.queue.lock().await;
                q.enqueue(u.clone(), title, duration);
            }
            direct += 1;
        } else {
            errors.push((
                u.clone(),
                outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "no videos extracted".to_string()),
            ));
        }
    }

    // Anything directly enqueued needs the worker woken + a queue swap + persist.
    if direct > 0 {
        state.notify.notify_one();
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
        drop(q);
        state.persist().await;
    }

    if !entries.is_empty() {
        let note = {
            let mut parts: Vec<String> = Vec::new();
            if direct > 0 {
                parts.push(format!(
                    "{direct} single-video URL{} queued directly",
                    if direct == 1 { "" } else { "s" }
                ));
            }
            for (u, m) in &errors {
                parts.push(format!("{}: {m}", u));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("; "))
            }
        };
        return render::render_approval(playlist_title.as_deref(), &entries, note.as_deref());
    }

    if direct > 0 {
        return format!(
            r#"<span class="ack">queued {direct} download{}</span>"#,
            if direct == 1 { "" } else { "s" }
        );
    }

    // Nothing enqueued, no playlist: surface the first error.
    let msg = errors
        .first()
        .map(|(_, m)| m.clone())
        .unwrap_or_else(|| "no videos extracted".to_string());
    format!(r#"<span class="err">{}</span>"#, render::esc(&msg))
}

/// POST /approve -- enqueue the per-video items the user ticked in an approval
/// list. Each checked checkbox carried a JSON `ApproveEntry` as its value;
/// we parse the raw urlencoded body ourselves with `form_urlencoded` (axum's
/// default `Form`/`serde_urlencoded` does not collapse repeated `entry` keys
/// into a `Vec`). Returns an ack fragment (into `#approve`), replacing the
/// approval list.
async fn post_approve(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> String {
    let entries: Vec<String> = form_urlencoded::parse(&body)
        .filter(|(k, _)| k == "entry")
        .map(|(_, v)| v.into_owned())
        .collect();

    if entries.is_empty() {
        return r#"<span class="err">select at least one video</span>"#.to_string();
    }

    let mut n: u64 = 0;
    {
        let mut q = state.queue.lock().await;
        for raw in &entries {
            match serde_json::from_str::<ApproveEntry>(raw) {
                Ok(e) => {
                    q.enqueue(e.url, e.title, e.duration);
                    n += 1;
                }
                Err(_) => {
                    // Skip a malformed value rather than failing the whole
                    // batch; the user can re-approve.
                }
            }
        }
    }
    state.notify.notify_one();
    {
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
    }
    state.persist().await;

    format!(
        r#"<span class="ack">queued {n} download{}</span>"#,
        if n == 1 { "" } else { "s" }
    )
}

// ------------------------------ /cancel/:id --------------------------------

/// POST /cancel/:id -- cancel the active or pending queue item.
async fn post_cancel(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> String {
    enum Outcome {
        PendingRemoved,
        ActiveSignalled,
        Noop,
    }

    let outcome = {
        let mut q = state.queue.lock().await;
        let Some(item) = q.get(id) else {
            return format!(r#"<span id="ack">no such item {id}</span>"#);
        };
        match item.status {
            ItemStatus::Pending => {
                q.remove_pending(id);
                Outcome::PendingRemoved
            }
            ItemStatus::Active => {
                if let Some(item) = q.get_mut(id) {
                    if let Some(tok) = &item.cancel {
                        tok.cancel();
                    }
                }
                Outcome::ActiveSignalled
            }
            _ => Outcome::Noop,
        }
    };

    match outcome {
        Outcome::PendingRemoved => {
            let q = state.queue.lock().await;
            state.emit(Event::Queue(render::render_queue(&q)));
            state.persist().await;
            format!(r#"<span id="ack">cancelled item {id}</span>"#)
        }
        Outcome::ActiveSignalled => {
            format!(r#"<span id="ack">cancelling item {id} ...</span>"#)
        }
        Outcome::Noop => {
            format!(r#"<span id="ack">item {id} not active</span>"#)
        }
    }
}

// ------------------------------ /retry/:id ---------------------------------

/// POST /retry/:id -- re-enqueue a cancelled/failed item at the back.
async fn post_retry(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> String {
    let result = {
        let mut q = state.queue.lock().await;
        // Reject retrying the active item.
        if let Some(item) = q.get(id) {
            if item.status == ItemStatus::Active {
                return format!(
                    r#"<span id="ack" class="err">item {id} is active; cancel it first</span>"#
                );
            }
            if item.status == ItemStatus::Done {
                return format!(
                    r#"<span id="ack" class="err">item {id} already downloaded; delete the file to re-download</span>"#
                );
            }
        } else {
            return format!(r#"<span id="ack" class="err">no such item {id}</span>"#);
        }
        // Reset to Pending, move to back, clear progress + error.
        if let Some(item) = q.get_mut(id) {
            item.status = ItemStatus::Pending;
            item.progress = None;
            item.error = None;
            item.filename = None;
            item.cancel = None;
        }
        // Move to back of pending run by reordering within items: extract and push.
        if let Some(pos) = q.items.iter().position(|i| i.id == id) {
            let item = q.items.remove(pos);
            q.items.push(item);
        }
    };
    let _ = result;
    state.notify.notify_one();
    {
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
    }
    state.persist().await;
    format!(r#"<span id="ack">requeued item {id}</span>"#)
}

// ------------------------------ /clear -------------------------------------

/// POST /clear -- drop all done/failed/cancelled items.
async fn post_clear(State(state): State<Arc<AppState>>) -> String {
    let n = {
        let mut q = state.queue.lock().await;
        q.clear_terminal()
    };
    {
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
    }
    state.persist().await;
    format!(r#"<span id="ack">cleared {n} item{}</span>"#, if n == 1 { "" } else { "s" })
}

// ------------------------------ /events (SSE) ------------------------------

/// GET /events -- long-lived global SSE stream. Emits a snapshot on connect
/// (queue, status, library, replayed log lines), then forwards live events.
async fn get_events(State(state): State<Arc<AppState>>) -> Response {
    let mut rx = state.events.subscribe();
    let shutdown = state.shutdown.clone();

    // Build the snapshot under the locks, *before* the stream starts.
    let (queue_frag, status_frag, library_frag, log_lines) = {
        let q = state.queue.lock().await;
        let active = q.items.iter().find(|i| i.status == ItemStatus::Active).cloned();
        let queue_frag = render::render_queue(&q);
        let status_frag = render::render_status(active.as_ref());
        drop(q);

        let library_frag = render::render_library_scan(&state.cfg.download_dir);

        let log_lines = {
            let ring = state.log_ring.lock().await;
            render::snapshot_log_lines(&ring.snapshot())
        };
        (queue_frag, status_frag, library_frag, log_lines)
    };

    let s = stream! {
        // --- snapshot ---
        yield Ok::<SseEvent, std::convert::Infallible>(
            SseEvent::default().event("queue").data(queue_frag)
        );
        yield Ok(SseEvent::default().event("status").data(status_frag));
        yield Ok(SseEvent::default().event("library").data(library_frag));
        for line in log_lines {
            yield Ok(SseEvent::default().event("log").data(line));
        }

        // --- live events ---
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                ev = rx.recv() => {
                    match ev {
                        Ok(event) => {
                            yield Ok(SseEvent::default()
                                .event(event.name())
                                .data(event.data()));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Re-snapshot to self-heal.
                            tracing::debug!("SSE lagged by {n}; re-snapshotting");
                            let q = state.queue.lock().await;
                            yield Ok(SseEvent::default()
                                .event("queue").data(render::render_queue(&q)));
                            let ring = state.log_ring.lock().await;
                            for line in render::snapshot_log_lines(&ring.snapshot()) {
                                yield Ok(SseEvent::default().event("log").data(line));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    };

    Sse::new(s)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Spawn the worker (kept here so `main` only depends on `server` + `config`).
/// Returns the worker's `JoinHandle` so `main` can await clean shutdown.
pub fn spawn_worker(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    worker::spawn(state)
}
