//! Axum router, route handlers, and the SSE stream.
use std::sync::Arc;

use async_stream::stream;
use axum::body::Body;
use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::events::Event;
use crate::library;
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

/// POST /download -- append URLs to the global queue; return an ack fragment.
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
        return r#"<span id="ack" class="err">paste at least one URL</span>"#.to_string();
    }

    let n = urls.len();
    {
        let mut q = state.queue.lock().await;
        for u in urls {
            q.enqueue(u);
        }
    }
    state.notify.notify_one();

    // Emit a fresh queue fragment so all tabs update.
    {
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
    }
    state.persist().await;

    format!(r#"<span id="ack">added {n} to queue</span>"#)
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
                            log::debug!("SSE lagged by {n}; re-snapshotting");
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
