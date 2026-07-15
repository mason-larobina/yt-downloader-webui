//! Axum router, route handlers, and the SSE stream.
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, LazyLock, Mutex};

use async_stream::stream;
use axum::body::{Body, Bytes};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::events::Event;
use crate::library;
use crate::render;
use crate::state::{AppState, EnqueueResult, ItemStatus};
use crate::worker;

use sha1::{Digest, Sha1};

/// Build the application router.
pub fn router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(index))
        .route("/static/htmx.org-2.0.4.js", axum::routing::get(static_htmx))
        .route(
            "/static/htmx-ext-sse-2.2.4.js",
            axum::routing::get(static_sse),
        )
        .route("/static/app.css", axum::routing::get(static_css))
        .route("/static/icons/{name}", axum::routing::get(static_icon))
        .route("/favicon.svg", axum::routing::get(favicon))
        .route("/favicon.ico", axum::routing::get(favicon))
        .route("/download", axum::routing::post(post_download))
        .route("/probe", axum::routing::get(get_probe))
        .route("/header", axum::routing::get(get_header))
        .route("/confirm", axum::routing::post(post_confirm))
        .route("/cancel/{id}", axum::routing::post(post_cancel))
        .route("/retry/{id}", axum::routing::post(post_retry))
        .route("/rescan", axum::routing::post(post_rescan))
        .route("/file/{name}", axum::routing::get(library::get_file))
        .route("/thumb/{name}", axum::routing::get(get_thumb))
        .route("/delete/{name}", axum::routing::post(library::delete_file))
        .route("/logs/{id}", axum::routing::get(get_logs))
        .route("/item/{id}", axum::routing::get(get_item_page))
        .route("/delete-item/{id}", axum::routing::post(post_delete_item))
        .route("/events", axum::routing::get(get_events))
        .with_state(state)
}

// ------------------------------ static -------------------------------------

const INDEX_HTML: &str = include_str!("../static/index.html");
// Vendored third-party JS (non-minified so it's readable / debuggable in the
// browser). Versions are pinned in the filenames so upgrading htmx also busts
// any browser cache, so these are served as `immutable`. Sources:
//   htmx.org-2.0.4.js      <- https://unpkg.com/htmx.org@2.0.4/dist/htmx.js
//   htmx-ext-sse-2.2.4.js  <- https://unpkg.com/htmx-ext-sse@2.2.4/dist/sse.js
const HTMX_JS: &[u8] = include_bytes!("../static/vendored/htmx.org-2.0.4.js");
const HTMX_SSE_JS: &[u8] = include_bytes!("../static/vendored/htmx-ext-sse-2.2.4.js");
const APP_CSS: &str = include_str!("../static/app.css");
/// The site favicon, derived from `static/download.svg` (the download glyph on
/// the app's violet->blue brand gradient). Served from both `/favicon.svg`
/// and `/favicon.ico` so a browser's default `/favicon.ico` probe resolves
/// too. `no-cache` + ETag (see `serve_embedded`) picks up an upgrade without
/// a version-pinned URL.
const FAVICON_SVG: &str = include_str!("../static/favicon.svg");

/// Overlay-button icons, embedded at compile time and served one each from
/// `/static/icons/{name}` so a card references them by URL instead of
/// inlining the full SVG markup on every card (which duplicated the bytes
/// once per video). Keyed by filename including the `.svg` suffix.
const ICONS: &[(&str, &str)] = &[
    ("download.svg", include_str!("../static/download.svg")),
    ("play.svg", include_str!("../static/play.svg")),
    ("trash.svg", include_str!("../static/trash.svg")),
    ("logs.svg", include_str!("../static/logs.svg")),
    ("stop.svg", include_str!("../static/stop.svg")),
    ("retry.svg", include_str!("../static/retry.svg")),
];

/// Cache policy for the compile-time-embedded entry document and CSS.
/// Their filenames are *not* version-pinned (unlike the vendored JS), so an
/// upgrade ships new bytes under the same URL -- hence `no-cache`, which
/// forces the browser to revalidate every load. A stable `ETag` (sha1 of the
/// embedded bytes) makes those revalidations cheap 304s instead of full
/// re-downloads. `index.html` must never be served stale: a stale index
/// references the *old* pinned JS filenames, which no longer exist on a new
/// binary and would 404.
const CACHE_NO_CACHE: &str = "no-cache";
/// Cache policy for assets whose URL is immutable for the life of the binary:
/// the version-pinned vendored JS (`/static/htmx*`) and the content-addressed
/// thumbnails (`/thumb/<sha1>.<ext>`). `immutable` lets browsers keep them
/// indefinitely without revalidating.
const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Cache policy for the compile-time-embedded overlay icons: stable names but
/// only a 1-day window so an upgrade clears stale icons reasonably soon
/// without forcing revalidation on every nav. The icon set is tiny, so this
/// conservative TTL costs nothing.
const CACHE_ICONS: &str = "public, max-age=86400";

/// Memoized ETags for compile-time-embedded assets, keyed by the asset's
/// `&'static` slice address (stable for the program lifetime, so the pointer
/// is a safe cache key). Computing sha1 of e.g. the 165 KB htmx bundle on every
/// request would be wasteful; the hash is computed once and reused.
static ASSET_ETAGS: LazyLock<Mutex<HashMap<usize, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Return a strong, quoted ETag (sha1 hex of `data`) for a compile-time-
/// embedded asset, computing it once and memoizing it for subsequent requests.
fn asset_etag(data: &'static [u8]) -> String {
    let key = data.as_ptr() as usize;
    let mut map = ASSET_ETAGS.lock().expect("asset etag map poisoned");
    if let Some(tag) = map.get(&key) {
        return tag.clone();
    }
    let mut h = Sha1::new();
    h.update(data);
    let tag = format!("\"{}\"", crate::thumb::hex(&h.finalize()));
    map.insert(key, tag.clone());
    tag
}

/// Serve a compile-time-embedded asset with a memoized `ETag`, honoring
/// `If-None-Match` (returns 304 on a match) and the given `Cache-Control`.
/// `content_type` is a `&'static str` so it can be inserted via
/// `HeaderValue::from_static` (no allocation).
fn serve_embedded(
    data: &'static [u8],
    content_type: &'static str,
    cache_control: &'static str,
    req: &HeaderMap,
) -> Response {
    let etag = asset_etag(data);
    if let Some(inm) = req.get(header::IF_NONE_MATCH)
        && inm.as_bytes() == etag.as_bytes()
    {
        let mut headers = HeaderMap::new();
        headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        );
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    (StatusCode::OK, headers, Body::from(data)).into_response()
}

/// Wrap a dynamically-rendered HTML fragment with `Cache-Control: no-store` so
/// an intermediary or browser can't heuristically cache a polled fragment
/// (e.g. `/logs/:id`, polled every 2s) and serve a stale swap.
fn no_store_html(body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (StatusCode::OK, headers, body).into_response()
}

async fn index(State(_state): State<Arc<AppState>>, req: HeaderMap) -> Response {
    // Static page; all dynamic state arrives via SSE. `no-cache` + ETag so a
    // returning browser after an upgrade never renders a stale index that
    // references the old (now-404) pinned JS filenames.
    serve_embedded(
        INDEX_HTML.as_bytes(),
        "text/html; charset=utf-8",
        CACHE_NO_CACHE,
        &req,
    )
}

async fn static_htmx(req: HeaderMap) -> Response {
    serve_embedded(
        HTMX_JS,
        "application/javascript; charset=utf-8",
        CACHE_IMMUTABLE,
        &req,
    )
}

async fn static_sse(req: HeaderMap) -> Response {
    serve_embedded(
        HTMX_SSE_JS,
        "application/javascript; charset=utf-8",
        CACHE_IMMUTABLE,
        &req,
    )
}

async fn static_css(req: HeaderMap) -> Response {
    serve_embedded(
        APP_CSS.as_bytes(),
        "text/css; charset=utf-8",
        CACHE_NO_CACHE,
        &req,
    )
}

/// GET /favicon.svg, GET /favicon.ico -- serve the embedded site favicon
/// (an SVG derived from `download.svg`). Bound to both names so the default
/// browser `/favicon.ico` probe resolves; the explicit `<link rel="icon">`
/// tags in the page heads point at `/favicon.svg`.
async fn favicon(req: HeaderMap) -> Response {
    serve_embedded(
        FAVICON_SVG.as_bytes(),
        "image/svg+xml",
        CACHE_NO_CACHE,
        &req,
    )
}

/// GET /static/icons/:name -- serve one of the compile-time-embedded overlay
/// icon SVGs (see `ICONS`). Cacheable for a day with a memoized `ETag` for
/// cheap 304s; returns 404 for unknown names so the route can't be abused to
/// probe the filesystem.
async fn static_icon(Path(name): Path<String>, req: HeaderMap) -> Response {
    match ICONS.iter().find(|(n, _)| *n == name).map(|(_, b)| *b) {
        Some(body) => serve_embedded(body.as_bytes(), "image/svg+xml", CACHE_ICONS, &req),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ------------------------------ /download ----------------------------------

#[derive(Deserialize)]
pub struct DownloadForm {
    pub url: String,
}

/// POST /download -- the header form's submit target. Validates that a URL
/// was pasted and returns the probe-area shell (a fragment with its own
/// `sse-connect="/probe?url=…"` swapped into `#header-input`). The actual
/// probing + streaming happens on the GET /probe SSE stream; this handler
/// never blocks on yt-dlp and never enqueues anything.
///
/// On an empty URL it re-renders the input form with an inline error.
async fn post_download(
    State(_state): State<Arc<AppState>>,
    Form(form): Form<DownloadForm>,
) -> String {
    let url = form.url.trim();
    if url.is_empty() {
        return render::render_header_input(Some("paste a URL"));
    }
    render::render_probe_area(url)
}

/// GET /header -- return the normal header input form (used by cancel/done
/// buttons in the probe area to restore the header into `#header-input`).
async fn get_header() -> Response {
    no_store_html(render::render_header_input(None))
}

// ------------------------------ /probe -------------------------------------

/// Query params for GET /probe.
#[derive(Deserialize)]
struct ProbeQuery {
    pub url: String,
}

/// GET /probe -- the per-probe SSE stream that drives the header's pending
/// probe result area. Streams each yt-dlp output line as a `log` event
/// (appended into `#probe-stream`), then emits a single `result` event
/// carrying the confirm cards (or an error + Done button) into
/// `#probe-cards`, and closes. The probe runs inline in the stream so that a
/// client disconnect (cancel / navigate away) drops the stream and kills the
/// yt-dlp child via `kill_on_drop`.
async fn get_probe(State(state): State<Arc<AppState>>, Query(q): Query<ProbeQuery>) -> Response {
    let url = q.url;
    let probe = worker::probe_stream(state.clone(), url.clone());
    let s = stream! {
        let mut probe = Box::pin(probe);
        while let Some(ev) = probe.next().await {
            match ev {
                worker::ProbeEvent::Log(line) => {
                    yield Ok::<SseEvent, Infallible>(
                        SseEvent::default().event("log").data(render::render_log_line(&line)),
                    );
                }
                worker::ProbeEvent::Done(outcome) => {
                    let frag = render::render_probe_result(
                        &url,
                        &outcome.entries,
                        outcome.single.as_ref(),
                        outcome.error.as_deref(),
                    );
                    yield Ok(SseEvent::default().event("result").data(frag));
                    return;
                }
            }
        }
    };
    Sse::new(s).keep_alive(KeepAlive::default()).into_response()
}

/// POST /confirm -- enqueue the per-video items the user ticked on the probe
/// result cards. Each checked checkbox carried a JSON `ApproveEntry` as its
/// value; we parse the raw urlencoded body ourselves with `form_urlencoded`
/// (axum's default `Form`/`serde_urlencoded` does not collapse repeated
/// `entry` keys into a `Vec`). Returns the normal header input form (swapped
/// into `#header-input`), restoring the header for the next URL.
async fn post_confirm(State(state): State<Arc<AppState>>, body: Bytes) -> String {
    let entries: Vec<String> = form_urlencoded::parse(&body)
        .filter(|(k, _)| k == "entry")
        .map(|(_, v)| v.into_owned())
        .collect();

    if entries.is_empty() {
        return render::render_header_input(Some("select at least one video"));
    }

    // (item id, thumbnail URL) pairs to fetch in the background. The fetched
    // remote thumbnail is the *primary* thumbnail (highest quality); when none
    // is fetched, a native ffmpeg-extracted frame (generated after the download)
    // acts as the fallback primary.
    let mut thumbs: Vec<(u64, String)> = Vec::new();
    // Ids of already-downloaded URLs that were re-surfaced (de-duplicated):
    // the existing Done item was moved to the top of the grid + its
    // `enqueued_at` touched to now, but no new row / file was created. Used
    // to emit one summary toast so the de-dup isn't silently swallowed.
    let mut subsumed: Vec<u64> = Vec::new();
    {
        let mut q = state.queue.lock().await;
        for raw in &entries {
            match serde_json::from_str::<render::ApprovalEntry>(raw) {
                Ok(e) => match q.enqueue(e.url, e.title, e.duration) {
                    EnqueueResult::Added(id) => {
                        if let Some(t) = e.thumbnail {
                            thumbs.push((id, t));
                        }
                        // Prepend the new card at the top. Emitted inline (in
                        // entry order, not batched) so the live DOM order
                        // matches the `items` Vec order (rendered
                        // `iter().rev()` on refresh) even for a mixed batch of
                        // new + already-downloaded entries.
                        if let Some(item) = q.get(id) {
                            state.emit(Event::CardAdded(render::render_card(item)));
                        }
                    }
                    EnqueueResult::Subsumed(id) => {
                        subsumed.push(id);
                        // Move the re-surfaced card to the top: remove it
                        // from its old slot (an empty `card-<id>` payload
                        // deletes the node) then prepend it fresh (a
                        // `card-added` event). Targeted, so unrelated cards
                        // keep their DOM / hover state -- no full-grid swap
                        // (which would re-trigger every card's overlay
                        // fade-in). Emitted inline in entry order so the
                        // live order matches the Vec on refresh.
                        state.emit(Event::Card {
                            id,
                            html: String::new(),
                        });
                        if let Some(item) = q.get(id) {
                            state.emit(Event::CardAdded(render::render_card(item)));
                        }
                    }
                },
                Err(_) => {
                    // Skip a malformed value rather than failing the whole
                    // batch; the user can re-probe.
                }
            }
        }
    }
    state.notify.notify_one();
    emit_count_status(&state).await;
    state.persist().await;

    if !thumbs.is_empty() {
        spawn_thumbnail_fetches(state.clone(), thumbs);
    }

    // Surface the de-duplication so it isn't silently buried: one toast for
    // the whole batch (the shared `#ack` slot overwrites, so per-item toasts
    // would just clobber each other).
    if !subsumed.is_empty() {
        let msg = if subsumed.len() == 1 {
            "Already downloaded \u{2014} de-duplicated; moved to the top.".to_string()
        } else {
            format!(
                "De-duplicated {} already-downloaded video(s); moved the latest to the top.",
                subsumed.len()
            )
        };
        state.emit(Event::Toast(render::render_ack(&msg, false)));
    }

    render::render_header_input(None)
}

// ------------------------------ /cancel/:id --------------------------------

/// Spawn a background task that fetches a batch of remote thumbnails
/// concurrently (no lock held during network I/O), then sets each landed
/// filename on its item under one lock and emits a single `queue` swap +
/// persist. The fetched remote thumbnail is the *primary* thumbnail; failures
/// are non-fatal -- a native ffmpeg-extracted frame (generated after the
/// download completes via `import::reconcile`) acts as the fallback primary,
/// and the native frames always populate the item-page gallery regardless.
fn spawn_thumbnail_fetches(state: Arc<AppState>, items: Vec<(u64, String)>) {
    tokio::spawn(async move {
        let mut set = tokio::task::JoinSet::new();
        for (id, url) in items {
            let http = state.http.clone();
            let dir = state.cfg.cache_dir.clone();
            set.spawn(async move {
                let r = crate::thumb::fetch(&http, &dir, &url).await;
                (id, r)
            });
        }
        let mut landed: Vec<(u64, String)> = Vec::new();
        while let Some(res) = set.join_next().await {
            match res {
                Ok((id, Ok(fname))) => landed.push((id, fname)),
                Ok((id, Err(e))) => {
                    tracing::debug!("thumbnail fetch failed for item {id}: {e:#}");
                }
                Err(e) => tracing::debug!("thumbnail fetch task panicked: {e}"),
            }
        }
        if landed.is_empty() {
            return;
        }
        let mut q = state.queue.lock().await;
        let mut changed: Vec<u64> = Vec::new();
        for (id, fname) in &landed {
            if let Some(item) = q.get_mut(*id) {
                // The remote thumbnail is the highest-quality primary; set it
                // unless one already resolved (an earlier fetch, or a native
                // frame already landed as a fallback).
                if item.thumbnail.is_none() {
                    item.thumbnail = Some(fname.clone());
                    changed.push(*id);
                }
            }
        }
        // One targeted per-card swap per changed item -- not a full-grid
        // `queue` swap -- so the rest of the grid's DOM (and any `:hover`
        // state) survives a thumbnail landing.
        for id in &changed {
            if let Some(html) = q.get(*id).map(render::render_card) {
                state.emit(Event::Card { id: *id, html });
            }
        }
        // If one of the landed thumbnails belongs to the currently-active
        // item, refresh just the banner's thumbnail slot (a targeted
        // `status-thumb` swap) -- not a full `#status` swap, which would
        // recreate the progress bar / title / cancel button and flash the
        // thumbnail mid-download. (A thumbnail commonly lands *after* the
        // item has gone active, since the fetch is spawned at enqueue and
        // the worker picks the item up almost immediately.)
        if let Some(active) = q.items.iter().find(|i| i.status == ItemStatus::Active) {
            if changed.contains(&active.id) {
                state.emit(Event::StatusThumb(
                    render::StatusParts::new(Some(active), 0).thumb,
                ));
            }
        }
        drop(q);
        state.persist().await;
    });
}

/// Emit a `cards-count` + `status-title` swap together. Used by request
/// handlers after mutating the queue so the floating banner's "N queued"
/// badge (and the header count) stay live alongside per-card changes. Only
/// the title slot is refreshed -- not the thumbnail / bar / meta / cancel
/// slots -- because these handlers never change the *active* item (they
/// enqueue / cancel-pending / retry-terminal / delete-terminal), so touching
/// the active-item slots would needlessly recreate the `<img>` thumbnail and
/// flash it mid-download. Active-item transitions are emitted by the worker's
/// `emit_final` + spawn block (which emit every slot).
async fn emit_count_status(state: &Arc<AppState>) {
    let q = state.queue.lock().await;
    let active = q.items.iter().find(|i| i.status == ItemStatus::Active);
    let pending = q
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Pending)
        .count();
    let total = q.items.len();
    state.emit(Event::CardsCount(render::render_cards_count(
        total, pending,
    )));
    state.emit(Event::StatusTitle(
        render::StatusParts::new(active, pending).title,
    ));
}

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
            return render::render_ack(&format!("no such item {id}"), false);
        };
        match item.status {
            ItemStatus::Pending => {
                q.remove_pending(id);
                Outcome::PendingRemoved
            }
            ItemStatus::Active => {
                if let Some(item) = q.get_mut(id)
                    && let Some(tok) = &item.cancel
                {
                    tok.cancel();
                }
                Outcome::ActiveSignalled
            }
            _ => Outcome::Noop,
        }
    };

    match outcome {
        Outcome::PendingRemoved => {
            // Remove the card from the grid with an empty `card-<id>` swap
            // (outerHTML of empty data deletes the node) -- no full-grid
            // re-render. The active-item cancel path emits its own card via
            // the worker's `emit_final`.
            state.emit(Event::Card {
                id,
                html: String::new(),
            });
            emit_count_status(&state).await;
            state.persist().await;
            render::render_ack(&format!("cancelled item {id}"), false)
        }
        Outcome::ActiveSignalled => render::render_ack(&format!("cancelling item {id} ..."), false),
        Outcome::Noop => render::render_ack(&format!("item {id} not active"), false),
    }
}

// ------------------------------ /retry/:id ---------------------------------

/// POST /retry/:id -- re-enqueue a cancelled/failed item at the back.
async fn post_retry(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> String {
    {
        let mut q = state.queue.lock().await;
        // Reject retrying the active item.
        if let Some(item) = q.get(id) {
            if item.status == ItemStatus::Active {
                return render::render_ack(&format!("item {id} is active; cancel it first"), true);
            }
            if item.status == ItemStatus::Done {
                return render::render_ack(
                    &format!("item {id} already downloaded; delete the file to re-download"),
                    true,
                );
            }
        } else {
            return render::render_ack(&format!("no such item {id}"), true);
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
    // Render the re-queued card (now Pending, at the back of `items` = top
    // of the newest-first grid) under the lock, then emit a remove + add pair
    // so the card moves to the top without a full-grid re-render: an empty
    // `card-<id>` swap deletes the old node (wherever it was), and a
    // `card-added` prepend inserts the fresh one at the top. Net effect equals
    // the position the full-grid render would have shown.
    let card_html = {
        let q = state.queue.lock().await;
        q.get(id).map(render::render_card)
    };
    state.notify.notify_one();
    if let Some(html) = card_html {
        state.emit(Event::Card {
            id,
            html: String::new(),
        });
        state.emit(Event::CardAdded(html));
    }
    emit_count_status(&state).await;
    state.persist().await;
    render::render_ack(&format!("requeued item {id}"), false)
}

// ------------------------------ /rescan ----------------------------------

/// POST /rescan -- re-scan the download directory and reconcile it with the
/// queue: prune Done items whose file is gone, import unreferenced videos as
/// new Done items (cards), dedupe, and re-probe missing media. Runs the same
/// `import::reconcile` used on startup / after each download. It runs
/// concurrently (spawned) since it may ffprobe files; the cards grid refreshes
/// automatically over SSE via the `queue` event reconcile emits when anything
/// changes, so this just kicks it off and returns an ack.
async fn post_rescan(State(state): State<Arc<AppState>>) -> String {
    tokio::spawn(crate::import::reconcile(state.clone()));
    render::render_ack("rescanning…", false)
}

// ------------------------------ /thumb/:name ------------------------------

/// GET /thumb/:name -- stream a cached thumbnail inline (served from
/// `cfg.cache_dir`). `:name` must be a bare filename; the path is resolved and
/// asserted to stay inside the cache dir, otherwise 404 (never an error that
/// leaks whether a path outside the dir exists). Thumbnails are small, so no
/// range support -- just stream the bytes. The filename is content-addressed
/// (`<sha1>.<ext>`), so the URL is immutable for life of the cache entry --
/// served with `immutable`. The bare name doubles as the ETag, giving free
/// 304s on revalidation (and after a cache clear, the same bytes land under
/// the same name again).
async fn get_thumb(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    req: HeaderMap,
) -> Response {
    let path = match crate::thumb::resolve(&state.cfg.cache_dir, &name) {
        Some(p) => p,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    // The content-addressed name is the natural ETag; check it before the
    // disk read so a revalidation is a free 304 with no I/O.
    let etag = format!("\"{}\"", name);
    if let Some(inm) = req.get(header::IF_NONE_MATCH)
        && inm.as_bytes() == etag.as_bytes()
    {
        let mut headers = HeaderMap::new();
        headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_IMMUTABLE),
        );
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let content_type = mime_guess::from_path(&path)
        .first()
        .map(|m| m.essence_str().to_string())
        .unwrap_or_else(|| "image/jpeg".to_string());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type).unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"{}\"", name)).unwrap(),
    );
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_IMMUTABLE),
    );
    (StatusCode::OK, headers, Body::from(bytes)).into_response()
}

// ------------------------------ /logs/:id ---------------------------------

/// GET /logs/:id -- render the inner log-line divs for `id` (the body of
/// `#item-log-lines` on the details page, polled every 2s while the item is
/// in flight). Returns an empty `(no output yet)` body when the item has
/// been cleared from the queue, so the poll degrades gracefully instead of
/// swapping in a full-page fragment.
async fn get_logs(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> Response {
    let item = state.queue.lock().await.get(id).cloned();
    let body = match item {
        Some(item) => render::render_log_lines(&item.logs),
        None => render::render_log_lines(&[]),
    };
    // Polled every 2s while in flight; never serve a stale swap from a
    // heuristic cache.
    no_store_html(body)
}

// ------------------------------ /item/:id ---------------------------------

/// GET /item/:id -- the full standalone details page for one video:
/// full thumbnail + metadata + big View/Download/Delete action buttons +
/// the complete yt-dlp log output. Returns a `text/html` document (the page
/// loads htmx itself for log polling + the action buttons). When the item is
/// no longer in the queue (cleared / never existed), serves a minimal
/// "gone" page with a back link instead of a bare 404.
async fn get_item_page(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> Response {
    let item = state.queue.lock().await.get(id).cloned();
    let body = match item {
        Some(item) => render::render_item_page(&item),
        None => render::render_item_gone(),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // Standalone page whose contents (logs, status) change as the item
    // progresses; never serve a stale snapshot.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (StatusCode::OK, headers, body).into_response()
}

// ------------------------------ /delete-item/:id --------------------------

/// POST /delete-item/:id -- delete a Done item's downloaded file (if present)
/// and remove the (terminal) card from the queue. Used by the card overlay's
/// `delete` button. No-op for non-terminal / missing items (returns an ack).
async fn post_delete_item(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> String {
    let (filename, download_dir) = {
        let q = state.queue.lock().await;
        let item = match q.get(id) {
            Some(i) => i,
            None => return render::render_ack(&format!("no such item {id}"), false),
        };
        if !item.status.is_terminal() {
            return render::render_ack(
                &format!("item {id} is not finished; cancel it first"),
                true,
            );
        }
        (item.filename.clone(), state.cfg.download_dir.clone())
    };

    // Delete the file on disk (best-effort).
    let mut file_msg = String::new();
    if let Some(name) = &filename
        && let Some(path) = crate::library::resolve_safe(&download_dir, name)
        && let Err(e) = tokio::fs::remove_file(&path).await
    {
        file_msg = format!(" (file: {e})");
    }

    let removed = {
        let mut q = state.queue.lock().await;
        q.remove_terminal(id)
    };
    if removed {
        // Remove the card with an empty `card-<id>` swap (outerHTML of empty
        // data deletes the node) -- no full-grid re-render.
        state.emit(Event::Card {
            id,
            html: String::new(),
        });
        emit_count_status(&state).await;
        state.persist().await;
    }
    render::render_ack(&format!("deleted item {id}{file_msg}"), false)
}

// ------------------------------ /events (SSE) ------------------------------

/// Build an SSE event from a name + data payload, ensuring the wire format
/// always carries a `data:` field.
///
/// axum's `SseEvent::data("")` short-circuits in its internal `write_buf`
/// before emitting the `data: ` prefix (it returns early on an empty buffer),
/// so an empty payload produces `event: <name>\n\n` with **no `data:` line**.
/// Per the SSE spec the browser's `EventSource` does not dispatch an event
/// whose data buffer is empty (the dispatch algorithm returns early), so
/// htmx's `sse-swap` listener never fires and the target slot is never
/// cleared. This is why the banner kept showing stale content when the
/// active item completed: `emit_final` emits empty `status-*` payloads to
/// clear the slots, but the browser silently dropped them.
///
/// The fix: for an empty payload, emit a minimal HTML comment `<!---->`
/// instead. It is non-empty on the wire (so the event dispatches and htmx
/// swaps it in), produces only a comment node in the DOM (so there is no
/// visible content), and CSS `:empty` still matches (per MDN: "Comments,
/// processing instructions, and CSS content do not affect whether an element
/// is considered empty"). This is centralized here so every event -- status
/// slot clears, empty `card-<id>` removals, empty `cards-count` -- benefits.
fn sse_event(name: std::borrow::Cow<'static, str>, data: &str) -> SseEvent {
    let data = if data.is_empty() { "<!---->" } else { data };
    SseEvent::default().event(name).data(data)
}

/// GET /events -- long-lived global SSE stream. Emits a snapshot on connect
/// (queue, status, library, replayed log lines), then forwards live events.
async fn get_events(State(state): State<Arc<AppState>>) -> Response {
    let mut rx = state.events.subscribe();
    let shutdown = state.shutdown.clone();

    // Build the snapshot under the locks, *before* the stream starts.
    let (queue_frag, status_evts, log_lines) = {
        let q = state.queue.lock().await;
        let active = q
            .items
            .iter()
            .find(|i| i.status == ItemStatus::Active)
            .cloned();
        let pending = q
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Pending)
            .count();
        let queue_frag = render::render_queue(&q);
        // The six banner slots, as ready-to-emit events (each targets one
        // stable slot in the `#status` shell -- see index.html).
        let status_evts = render::status_events(active.as_ref(), pending);
        drop(q);

        let log_lines = {
            let ring = state.log_ring.lock().await;
            render::snapshot_log_lines(&ring.snapshot())
        };
        (queue_frag, status_evts, log_lines)
    };

    let s = stream! {
        // --- snapshot ---
        yield Ok::<SseEvent, std::convert::Infallible>(
            sse_event("queue".into(), &queue_frag)
        );
        for ev in &status_evts {
            yield Ok(sse_event(ev.name(), ev.data()));
        }
        for line in log_lines {
            yield Ok(sse_event("log".into(), &line));
        }

        // --- live events ---
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                ev = rx.recv() => {
                    match ev {
                        Ok(event) => {
                            yield Ok(sse_event(event.name(), event.data()));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Re-snapshot to self-heal: the full cards grid,
                            // the six banner slots, and the replayed log
                            // lines. (The banner slots were missing from the
                            // old lag-recovery, leaving the banner stale after
                            // a lag.)
                            tracing::debug!("SSE lagged by {n}; re-snapshotting");
                            let q = state.queue.lock().await;
                            yield Ok(sse_event("queue".into(), &render::render_queue(&q)));
                            let active = q
                                .items
                                .iter()
                                .find(|i| i.status == ItemStatus::Active);
                            let pending = q
                                .items
                                .iter()
                                .filter(|i| i.status == ItemStatus::Pending)
                                .count();
                            for ev in render::status_events(active, pending) {
                                yield Ok(sse_event(ev.name(), ev.data()));
                            }
                            drop(q);
                            let ring = state.log_ring.lock().await;
                            for line in render::snapshot_log_lines(&ring.snapshot()) {
                                yield Ok(sse_event("log".into(), &line));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    };

    Sse::new(s).keep_alive(KeepAlive::default()).into_response()
}

/// Spawn the worker (kept here so `main` only depends on `server` + `config`).
/// Returns the worker's `JoinHandle` so `main` can await clean shutdown.
pub fn spawn_worker(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    worker::spawn(state)
}
