//! Render server-side HTML fragments for SSE events.
use crate::events::Event;
use crate::media::MediaInfo;
use crate::parse::FlatEntry;
use crate::state::{ItemStatus, Queue, QueueItem};

/// JSON shape embedded in each probe-result card checkbox `value`, so POST
/// /confirm can reconstruct per-video items (with titles + thumbnail URLs)
/// without any server-side stash. The whole blob is HTML-escaped into the
/// attribute; the browser decodes it back to this JSON on form submit.
///
/// This is the single source of truth for the render (serialize) <-> server
/// (deserialize) round-trip contract; both sides use this one type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovalEntry {
    pub url: String,
    pub title: Option<String>,
    pub duration: Option<f64>,
    /// Best-thumbnail URL harvested by the probe; POST /confirm fetches it into
    /// the cache and attaches the resulting filename to the enqueued item.
    pub thumbnail: Option<String>,
}

/// HTML-escape untrusted text (log lines, URLs, filenames).
pub fn esc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Format a byte count human-readably.
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if n == 0 {
        return "0 B".to_string();
    }
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < UNITS.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", f, UNITS[i])
    }
}

/// Format a speed in bytes/sec.
fn human_speed(bytes_per_sec: Option<f64>) -> String {
    match bytes_per_sec {
        Some(s) if s > 0.0 => format!("{}/s", human_bytes(s as u64)),
        _ => String::new(),
    }
}

/// Format an eta (seconds) as MM:SS or H:MM:SS.
fn human_eta(eta: Option<f64>) -> String {
    match eta {
        Some(e) if e >= 0.0 => {
            let s = e as u64;
            let h = s / 3600;
            let m = (s % 3600) / 60;
            let sec = s % 60;
            if h > 0 {
                format!("{}:{:02}:{:02}", h, m, sec)
            } else {
                format!("{:02}:{:02}", m, sec)
            }
        }
        _ => String::new(),
    }
}

/// Format a video duration (seconds) as M:SS or H:MM:SS. `None` -> empty.
fn human_duration(secs: Option<f64>) -> String {
    match secs {
        Some(e) if e > 0.0 => {
            let s = e as u64;
            let h = s / 3600;
            let m = (s % 3600) / 60;
            let sec = s % 60;
            if h > 0 {
                format!("{}:{:02}:{:02}", h, m, sec)
            } else {
                format!("{}:{:02}", m, sec)
            }
        }
        _ => String::new(),
    }
}

// ----------------------------- #status ------------------------------------

use askama::Template;

/**
 * Render the `#status` banner fragments. The banner is a stable 3-column
 * shell rendered once in `index.html` (left = thumbnail, middle = 4 lines:
 * title / last log / progress bar / progress text, right = cancel button);
 * each slot is its own `sse-swap` target so a progress tick swaps only the
 * bar + meta fragments -- never the `<img>` thumbnail, which is what flashed
 * when the whole `#status` `outerHTML` was swapped several times a second.
 *
 * Each field below is the **innerHTML** of one slot (an empty string
 * collapses the slot via CSS `:empty`; an empty `title` hides the whole
 * banner -- the idle state). [`status_events`] wraps the six fields as
 * ready-to-emit [`Event`]s for the snapshot / structural-transition paths;
 * the hot path builds a `StatusParts` and emits only the fields that
 * actually changed (bar + meta on a progress tick, log on a log line).
 *
 * `pending` is the number of Pending items (drives the live queued badge).
 */
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusParts {
    /// `<img class="bn-img" ...>` / placeholder `<div>` / "".
    pub thumb: String,
    /// `<span class="bn-title">…</span>` (+ optional queued badge) / "".
    pub title: String,
    /// `<div class="bn-log">…</div>` / "".
    pub log: String,
    /// `<i style="width:X%"></i>` / "".
    pub bar: String,
    /// `<span class="bn-left">…</span><span class="bn-pct">…</span><span
    /// class="bn-right">…</span>` / "".
    pub meta: String,
    /// `<button class="bn-cancel" hx-post="/cancel/<id>">…</button>` / "".
    pub cancel: String,
}

impl StatusParts {
    /// Compute the six banner-slot fragments for the current `(active,
    /// pending)` state. Cheap enough to call on every throttled progress tick;
    /// callers emit only the subset that actually changed.
    pub fn new(active: Option<&QueueItem>, pending: usize) -> Self {
        // Fully idle: nothing active and nothing queued. Every slot empty --
        // the empty `title` hides the whole banner via CSS.
        if active.is_none() && pending == 0 {
            return Self::default();
        }

        // Queued: nothing active yet, but items are waiting. Show only the
        // title line ("Waiting… N queued"); the other slots stay empty so
        // their columns / lines collapse and the banner reads as a compact
        // one-liner.
        if active.is_none() {
            return Self {
                title: format!(
                    "<span class=\"bn-title\">Waiting&hellip;</span>\
                     <span class=\"bn-queued\">{pending} queued</span>"
                ),
                ..Self::default()
            };
        }

        let item = active.unwrap();
        let p = item.progress.as_ref();
        let percent = p.and_then(|p| p.percent()).unwrap_or(0.0);
        let width = percent.clamp(0.0, 100.0);
        let speed = human_speed(p.and_then(|p| p.speed));
        let eta = human_eta(p.and_then(|p| p.eta));
        let bytes = p.and_then(|p| {
            let dl = p.downloaded_bytes? as u64;
            let tot = p.total_bytes.or(p.total_bytes_estimate).map(|x| x as u64);
            Some(match tot {
                Some(tot) if tot > 0 => format!("{} / {}", human_bytes(dl), human_bytes(tot)),
                _ => human_bytes(dl),
            })
        });
        let dur = human_duration(item.duration);

        let mut left_bits: Vec<String> = Vec::new();
        if !dur.is_empty() {
            left_bits.push(dur);
        }
        if !eta.is_empty() {
            left_bits.push(format!("ETA {eta}"));
        }
        let mut right_bits: Vec<String> = Vec::new();
        if !speed.is_empty() {
            right_bits.push(speed);
        }
        if let Some(b) = bytes.as_ref().filter(|b| !b.is_empty()) {
            right_bits.push(b.clone());
        }

        let thumb = match item.thumbnail.as_deref() {
            Some(name) => format!(
                "<img class=\"bn-img\" src=\"/thumb/{}\" alt=\"\" loading=\"lazy\">",
                esc(name)
            ),
            None => r#"<div class="bn-img bn-img-placeholder"></div>"#.to_string(),
        };

        let mut title = format!("<span class=\"bn-title\">{}</span>", esc(item.label()));
        if pending > 0 {
            title.push_str(&format!(
                "<span class=\"bn-queued\">{pending} queued</span>"
            ));
        }

        let log = item
            .logs
            .last()
            .map(|l| {
                format!(
                    "<div class=\"bn-log\">{}</div>",
                    esc(l.trim_end_matches('\n'))
                )
            })
            .unwrap_or_default();

        let bar = format!("<i style=\"width:{width:.0}%\"></i>");

        let meta = format!(
            "<span class=\"bn-left\">{}</span>\
             <span class=\"bn-pct\">{percent:.0}%</span>\
             <span class=\"bn-right\">{}</span>",
            left_bits.join(" &middot; "),
            right_bits.join(" &middot; "),
        );

        let cancel = format!(
            "<button class=\"bn-cancel\" hx-post=\"/cancel/{id}\" \
             hx-target=\"#ack\" hx-swap=\"innerHTML\" \
             title=\"cancel download\">\
             <img class=\"bn-cancel-icon\" src=\"/static/icons/stop.svg\" \
             alt=\"\" width=\"16\" height=\"16\" loading=\"lazy\">\
             <span class=\"bn-cancel-text\">cancel</span></button>",
            id = item.id,
        );

        Self {
            thumb,
            title,
            log,
            bar,
            meta,
            cancel,
        }
    }
}

/// The six banner-slot fragments wrapped as ready-to-emit [`Event`]s, in slot
/// order (thumb, title, log, bar, meta, cancel). Used by the snapshot (SSE
/// connect), lag-recovery, and every structural transition (active item
/// change, idle↔queued↔active) -- i.e. every path that may change *any* slot.
/// The progress-tick / log-line hot path builds a [`StatusParts`] directly and
/// emits only the one or two fields that actually changed.
pub fn status_events(active: Option<&QueueItem>, pending: usize) -> Vec<Event> {
    let p = StatusParts::new(active, pending);
    vec![
        Event::StatusThumb(p.thumb),
        Event::StatusTitle(p.title),
        Event::StatusLog(p.log),
        Event::StatusBar(p.bar),
        Event::StatusMeta(p.meta),
        Event::StatusCancel(p.cancel),
    ]
}

// ----------------------------- #cards --------------------------------------

/// View model for one video card. `dur` is pre-formatted (or empty).
#[derive(Template)]
#[template(path = "card.html")]
struct Card<'a> {
    id: u64,
    status: &'a str,
    label: &'a str,
    thumb: Option<&'a str>,
    filename: Option<&'a str>,
    dur: String,
    error: Option<&'a str>,
}

impl<'a> Card<'a> {
    fn from_item(item: &'a QueueItem) -> Self {
        Card {
            id: item.id,
            status: item.status.as_str(),
            label: item.label(),
            thumb: item.thumbnail.as_deref(),
            filename: item.filename.as_deref(),
            dur: human_duration(item.duration),
            error: item.error.as_deref(),
        }
    }
}

/// Render one video card. Used both by the live targeted-SSE path
/// ([`Event::Card`] / [`Event::CardAdded`]) and by the full-grid snapshot
/// ([`render_queue`]); the `card.html` template root carries the stable
/// `id="card-<id>"` + `sse-swap="card-<id>"` + `hx-swap="outerHTML"`
/// so a card swapped in by *any* path re-binds its own listener via htmx
/// reprocessing.
pub fn render_card(item: &QueueItem) -> String {
    Card::from_item(item).render().unwrap_or_default()
}

/// The `"N total, M pending"` count text swapped into `#cards-count`
/// (via the `cards-count` SSE event). Empty when the queue is empty so the
/// span collapses (matching the old full-grid render, which omitted the
/// count entirely for an empty queue).
pub fn render_cards_count(total: usize, pending: usize) -> String {
    if total == 0 {
        String::new()
    } else {
        format!("{total} total, {pending} pending")
    }
}

/// Render the cards-pane inner fragment (swapped into `#cards` via the
/// `queue` SSE event on connect / lag-recovery only). The header splits into
/// a static title + a `#cards-count` span (itself an `sse-swap` target for
/// the `cards-count` event), and `#cards-list` is the `card-added`
/// `afterbegin` prepend target. Cards are rendered newest-first (most
/// recently enqueued at the top) so the latest activity is visible without
/// scrolling; the `"no videos yet"` empty hint is always present and hidden
/// via CSS (`.cards-list:has(.card) .empty`) when a card exists, so removing
/// the last card needs no extra event.
#[derive(Template)]
#[template(path = "queue.html")]
struct QueueView<'a> {
    count: String,
    cards: Vec<Card<'a>>,
}

pub fn render_queue(queue: &Queue) -> String {
    let pending = queue
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Pending)
        .count();
    let total = queue.items.len();

    // Newest first: iterate the queue (FIFO by enqueue time) in reverse.
    let cards: Vec<Card<'_>> = queue.items.iter().rev().map(Card::from_item).collect();

    QueueView {
        count: render_cards_count(total, pending),
        cards,
    }
    .render()
    .unwrap_or_default()
}

// ----------------------------- #item (details page) ------------------------

/// The full standalone details page served at GET /item/:id. Renders the
/// full thumbnail, status badge, big text action buttons (View / Download /
/// Delete for a finished file; Cancel while in flight; Retry / Delete for a
/// terminal failure), the video's metadata, and the complete yt-dlp output
/// (polled while the item may still be producing lines).
#[derive(Template)]
#[template(path = "item.html")]
struct ItemPage<'a> {
    id: u64,
    status: &'a str,
    label: &'a str,
    url: &'a str,
    thumb: Option<&'a str>,
    /// All native thumbnail frames (cache basenames) for the gallery.
    thumbnails: &'a [String],
    filename: Option<&'a str>,
    dur: String,
    error: Option<&'a str>,
    enqueued: String,
    /// Pre-formatted media strings (or `None`) for the metadata table.
    media_video: Option<String>,
    media_audio: Option<String>,
    media_bitrate: Option<String>,
    media_format: Option<String>,
    media_size: Option<String>,
    /// Poll the log body only while the download may still emit output.
    polling: bool,
    lines: Vec<String>,
}

/// Render the full HTML document for GET /item/:id. Returns a complete
/// `<!DOCTYPE html>` page (the handler wraps it with the text/html headers).
pub fn render_item_page(item: &QueueItem) -> String {
    let lines: Vec<String> = item
        .logs
        .iter()
        .map(|l| l.trim_end_matches('\n').to_string())
        .collect();
    ItemPage {
        id: item.id,
        status: item.status.as_str(),
        // On the details page prefer the human title over the on-disk filename
        // (the filename is listed separately in the metadata) so the headline
        // reads as a title rather than a `video-12345.webm` slug.
        label: item
            .title
            .as_deref()
            .or(item.filename.as_deref())
            .unwrap_or(&item.url),
        url: &item.url,
        thumb: item.thumbnail.as_deref(),
        thumbnails: &item.thumbnails,
        filename: item.filename.as_deref(),
        dur: human_duration(item.duration),
        error: item.error.as_deref(),
        enqueued: item
            .enqueued_at
            .format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]"
            ))
            .unwrap_or_default(),
        media_video: item.media.as_ref().and_then(media_video_str),
        media_audio: item.media.as_ref().and_then(|m| m.audio_codec.clone()),
        media_bitrate: item.media.as_ref().and_then(|m| m.bitrate_str()),
        media_format: item.media.as_ref().and_then(|m| m.format_long_name.clone()),
        media_size: item.media.as_ref().and_then(|m| m.size_str()),
        polling: matches!(item.status, ItemStatus::Pending | ItemStatus::Active),
        lines,
    }
    .render()
    .unwrap_or_default()
}

/// Format the video-stream line: `h264 · 1920x1080 · 29.97 fps`. `None` if no
/// video codec was probed.
fn media_video_str(m: &MediaInfo) -> Option<String> {
    let codec = m.video_codec.clone()?;
    let mut bits: Vec<String> = vec![codec];
    if let Some(r) = m.resolution_str() {
        bits.push(r);
    }
    if let Some(fps) = m.fps {
        bits.push(format!("{fps:.2} fps"));
    }
    Some(bits.join(" \u{00b7} "))
}

/// The standalone "this video is no longer in the queue" page served by
/// GET /item/:id when the item has been cleared / never existed. Minimal HTML
/// with a back link (the page must still be a valid document, not a bare 404,
/// so the user isn't left on a blank tab).
#[derive(Template)]
#[template(path = "item_gone.html")]
struct ItemGone;

pub fn render_item_gone() -> String {
    ItemGone.render().unwrap_or_default()
}
#[derive(Template)]
#[template(path = "log_lines.html")]
struct LogLines<'a> {
    lines: Vec<&'a str>,
}

pub fn render_log_lines(lines: &[String]) -> String {
    let trimmed: Vec<&str> = lines.iter().map(|l| l.trim_end_matches('\n')).collect();
    LogLines { lines: trimmed }.render().unwrap_or_default()
}

// ----------------------------- #log ----------------------------------------

/// Render one escaped log line as a fragment to append.
#[derive(Template)]
#[template(path = "log_line.html")]
struct LogLine<'a> {
    line: &'a str,
}

pub fn render_log_line(line: &str) -> String {
    LogLine {
        line: line.trim_end_matches('\n'),
    }
    .render()
    .unwrap_or_default()
}

// ----------------------------- header / probe ------------------------------

/// Render the normal header input form (a single URL text field + Add
/// button). `error` is an optional inline message shown beneath the field.
#[derive(Template)]
#[template(path = "header_input.html")]
struct HeaderInput<'a> {
    error: Option<&'a str>,
}

pub fn render_header_input(error: Option<&str>) -> String {
    HeaderInput { error }.render().unwrap_or_default()
}

/// Render the pending probe result area: a shell with its own SSE connection
/// that streams probe log lines and swaps the final result cards.
#[derive(Template)]
#[template(path = "probe_area.html")]
struct ProbeArea<'a> {
    url: &'a str,
}

pub fn render_probe_area(url: &str) -> String {
    ProbeArea { url }.render().unwrap_or_default()
}

/// One probe-result card. `value` is the HTML-escaped JSON blob carried in
/// the checkbox `value` attribute (auto-escaped by the template).
#[derive(Template)]
#[template(path = "probe_result.html")]
struct ProbeResultView {
    cards: Vec<ProbeCard>,
    error_msg: String,
    n: usize,
}

struct ProbeCard {
    value: String,
    label: String,
    dur: String,
    thumb: Option<String>,
    idx: usize,
}

pub fn render_probe_result(
    submitted_url: &str,
    entries: &[FlatEntry],
    single: Option<&FlatEntry>,
    error: Option<&str>,
) -> String {
    // Build the list of ApprovalEntry to confirm.
    let cards: Vec<ApprovalEntry> = if !entries.is_empty() {
        entries
            .iter()
            .map(|e| ApprovalEntry {
                url: e.url.clone().unwrap_or_default(),
                title: e.title.clone(),
                duration: e.duration,
                thumbnail: e.thumbnail.clone(),
            })
            .collect()
    } else if let Some(sv) = single {
        vec![ApprovalEntry {
            url: submitted_url.to_string(),
            title: sv.title.clone(),
            duration: sv.duration,
            thumbnail: sv.thumbnail.clone(),
        }]
    } else {
        let msg = error
            .map(str::to_string)
            .unwrap_or_else(|| "no videos extracted".into());
        return ProbeResultView {
            cards: Vec::new(),
            error_msg: msg,
            n: 0,
        }
        .render()
        .unwrap_or_default();
    };

    let view_cards: Vec<ProbeCard> = cards
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let blob = serde_json::to_string(e).unwrap_or_default();
            ProbeCard {
                value: blob,
                label: e.title.clone().unwrap_or_else(|| e.url.clone()),
                dur: human_duration(e.duration),
                thumb: e.thumbnail.clone(),
                idx: i + 1,
            }
        })
        .collect();
    let n = view_cards.len();
    ProbeResultView {
        cards: view_cards,
        error_msg: String::new(),
        n,
    }
    .render()
    .unwrap_or_default()
}

// ----------------------------- snapshots -----------------------------------

/// Build the queue + log snapshot used by SSE on connect: returns the queue
/// fragment, then each log-line fragment (already-rendered).
pub fn snapshot_log_lines(lines: &[&String]) -> Vec<String> {
    lines.iter().map(|l| render_log_line(l)).collect()
}

// ----------------------------- #ack ---------------------------------------

/// A small ack fragment swapped into `#ack` by POST handlers. `err` adds the
/// `err` class. The message is HTML-escaped by the template.
#[derive(Template)]
#[template(path = "ack.html")]
struct Ack<'a> {
    msg: &'a str,
    err: bool,
}

pub fn render_ack(msg: &str, err: bool) -> String {
    Ack { msg, err }.render().unwrap_or_default()
}

#[cfg(test)]
mod probe_result_tests {
    use super::*;
    use crate::parse::FlatEntry;

    /// The checkbox `value` is an HTML-escaped JSON blob that POST /confirm
    /// must be able to deserialise back into `{url,title,duration,thumbnail}`.
    /// This pins the round-trip contract on the shared `ApprovalEntry` type.
    fn entry(
        url: &str,
        title: Option<&str>,
        duration: Option<f64>,
        thumbnail: Option<&str>,
    ) -> FlatEntry {
        FlatEntry {
            _type: Some("url".into()),
            url: Some(url.into()),
            title: title.map(str::to_string),
            duration,
            thumbnail: thumbnail.map(str::to_string),
            ..Default::default()
        }
    }

    /// Extract every checkbox `value` attribute (HTML-escaped JSON) from the
    /// rendered fragment, as a browser would surface it on form submit.
    fn checkbox_values(html: &str) -> Vec<String> {
        let needle = r#"name="entry" value=""#;
        html.match_indices(needle)
            .map(|(i, _)| {
                let start = i + needle.len();
                let rest = &html[start..];
                let end = rest.find('"').unwrap_or(rest.len());
                rest[..end].to_string()
            })
            .collect()
    }

    fn unescape(s: &str) -> String {
        s.replace("&#34;", "\"")
            .replace("&quot;", "\"")
            .replace("&#38;", "&")
            .replace("&amp;", "&")
            .replace("&#60;", "<")
            .replace("&lt;", "<")
            .replace("&#62;", ">")
            .replace("&gt;", ">")
            .replace("&#39;", "'")
    }

    /// A playlist probe yields one card per entry; the form posts to
    /// /confirm into `#header-input`; checkbox values round-trip with
    /// title/duration/thumbnail intact (including HTML-special title chars).
    #[test]
    fn playlist_result_round_trips() {
        let entries = vec![
            entry(
                "https://www.youtube.com/watch?v=aaa",
                Some("First & <second> \"quoted\""),
                Some(3623.0),
                Some("https://i.ytimg.com/vi/aaa/hqdefault.jpg"),
            ),
            entry("https://www.youtube.com/watch?v=bbb", None, None, None),
        ];
        let html = render_probe_result("https://ignored", &entries, None, None);

        assert!(
            html.contains(r##"hx-post="/confirm""##),
            "posts to /confirm"
        );
        assert!(html.contains(r##"hx-target="#header-input""##));
        assert!(
            html.contains(r##"hx-get="/header""##),
            "cancel restores header"
        );
        assert!(html.contains("Confirm ("));

        let values = checkbox_values(&html);
        assert_eq!(values.len(), 2, "expected 2 cards, got {values:?}");

        let first: ApprovalEntry =
            serde_json::from_str(&unescape(&values[0])).expect("first value decodes");
        assert_eq!(first.url, "https://www.youtube.com/watch?v=aaa");
        assert_eq!(first.title.as_deref(), Some("First & <second> \"quoted\""));
        assert_eq!(first.duration, Some(3623.0));
        assert_eq!(
            first.thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/aaa/hqdefault.jpg")
        );

        let second: ApprovalEntry =
            serde_json::from_str(&unescape(&values[1])).expect("second value decodes");
        assert_eq!(second.url, "https://www.youtube.com/watch?v=bbb");
        assert!(second.title.is_none());
        assert!(second.duration.is_none());
        assert!(second.thumbnail.is_none());
    }

    /// A single-video probe yields exactly one card whose checkbox `url` is
    /// the *original submitted URL* (not the probe dict's media `url`).
    #[test]
    fn single_video_result_uses_submitted_url() {
        let single = entry(
            "https://media.example/v/aaa", // media URL -- must NOT be used
            Some("Some Video"),
            Some(99.0),
            Some("https://i.ytimg.com/vi/aaa/hqdefault.jpg"),
        );
        let html = render_probe_result(
            "https://www.youtube.com/watch?v=aaa",
            &[],
            Some(&single),
            None,
        );
        let values = checkbox_values(&html);
        assert_eq!(values.len(), 1, "single video -> one card");
        let e: ApprovalEntry = serde_json::from_str(&unescape(&values[0])).expect("decodes");
        assert_eq!(e.url, "https://www.youtube.com/watch?v=aaa");
        assert_eq!(e.title.as_deref(), Some("Some Video"));
        assert_eq!(e.duration, Some(99.0));
    }

    /// A failed probe (no entries, no single) renders an error line + a Done
    /// button that restores the header -- no form, no checkboxes.
    #[test]
    fn error_result_shows_done_button() {
        let html = render_probe_result("https://x", &[], None, Some("Video unavailable"));
        assert!(html.contains(r#"class="err""#));
        assert!(html.contains("Video unavailable"));
        assert!(html.contains(r##"hx-get="/header""##));
        assert!(html.contains(">Done<"));
        assert!(html.contains("probe-error"));
        assert!(!html.contains("name=\"entry\""), "no checkboxes on error");
    }

    /// The probe-area shell wires its own SSE stream: `sse-connect` carries
    /// the percent-encoded URL (so `&`/`#`/`?`/`"` can't break the query param
    /// or the attribute), `sse-swap="log"` appends streaming lines, and
    /// `sse-swap="result"` + `sse-close="result"` swap+close on the final card.
    #[test]
    fn probe_area_shell_streams_and_closes() {
        let html = render_probe_area("https://www.youtube.com/watch?v=aaa&list=PL1&t=2");
        assert!(html.contains(r#"sse-connect="/probe?url="#), "sse-connect");
        // `&`, `=`, `?`, `:`, `/` all encoded in the query value.
        assert!(
            html.contains("https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Daaa%26list%3DPL1%26t%3D2"),
            "url encoded"
        );
        assert!(html.contains("sse-swap=\"log\""), "sse-swap log");
        assert!(html.contains("hx-swap=\"beforeend\""), "log lines append");
        assert!(html.contains("sse-swap=\"result\""), "sse-swap result");
        assert!(
            html.contains("sse-close=\"result\""),
            "stream closes on result"
        );
        assert!(
            html.contains("hx-get=\"/header\""),
            "cancel restores header"
        );
    }

    /// The header input form POSTs to /download into `#header-input` (so the
    /// input is replaced by the probe area on submit) and carries an inline
    /// error when given one.
    #[test]
    fn header_input_form_targets_download() {
        let ok = render_header_input(None);
        assert!(ok.contains(r##"hx-post="/download""##));
        assert!(ok.contains(r##"hx-target="#header-input""##));
        assert!(ok.contains(r#"name="url""#));
        assert!(!ok.contains("header-err"));

        let err = render_header_input(Some("paste a URL"));
        assert!(err.contains(r#"class="header-err""#));
        assert!(err.contains("paste a URL"));
    }
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use crate::state::{ItemStatus, QueueItem};

    fn item(status: ItemStatus, thumb: Option<&str>) -> QueueItem {
        let mut it = QueueItem::new(7, "https://example/watch?v=x".into());
        it.title = Some("Hello World".into());
        it.duration = Some(123.0);
        it.status = status;
        it.thumbnail = thumb.map(str::to_string);
        it
    }

    /// The card's badge + overlay buttons must be anchored to the thumbnail
    /// (`.card-thumb`, the only `position: relative` ancestor), not the
    /// viewport. Regression for the "overlay appeared at top-right of the
    /// viewport" bug caused by emitting the `<img>` without the wrapper.
    #[test]
    fn card_wraps_thumb_and_anchors_overlay() {
        let html = render_card(&item(ItemStatus::Done, Some("abc.jpg")));

        // The thumb image + badge + overlay are all inside one .card-thumb.
        let thumb_start = html
            .find("<div class=\"card-thumb\">")
            .expect("card-thumb wrapper");
        let thumb_end = html[thumb_start..]
            .find("</div>")
            .map(|e| thumb_start + e)
            .expect("card-thumb close");
        let thumb = &html[thumb_start..thumb_end];
        assert!(
            thumb.contains("<img class=\"card-img\""),
            "img inside thumb"
        );
        assert!(thumb.contains("card-overlay"), "overlay inside thumb");
        assert!(thumb.contains("card-badge"), "badge inside thumb");
    }

    /// The done card surfaces download/open/delete overlay buttons. The whole
    /// card is a stretched link to the details page (no separate "i" button).
    /// Buttons are icon-labelled (cached `<img>`s served from /static/icons),
    /// so we assert on their `title` tooltips (which also keep the actions
    /// accessible) and that an `<img>` is rendered for each.
    #[test]
    fn done_card_overlay_has_actions() {
        let mut it = item(ItemStatus::Done, None);
        it.filename = Some("video.webm".into());
        let html = render_card(&it);
        assert!(html.contains(r#"title="download to this device"#));
        assert!(html.contains(r#"title="open/preview"#));
        assert!(html.contains(r#"title="delete from server"#));
        // The card itself is a stretched link to the details page (the old
        // per-card "i" inspect button is gone).
        assert!(
            html.contains(r#"class="card-link" href="/item/7" target="_blank" rel="noopener""#),
            "stretched link opens details in a new tab"
        );
        assert!(
            !html.contains("/static/icons/info.svg"),
            "no inspect icon anymore"
        );
        // Each action embeds an <img> icon (download/open/delete = 3 icons).
        assert_eq!(
            html.matches("<img").count(),
            3,
            "download/open/delete icons"
        );
        assert!(
            html.contains(r#"src="/static/icons/download.svg"#),
            "download icon url"
        );
        assert!(
            html.contains(r#"src="/static/icons/play.svg"#),
            "open icon url"
        );
        assert!(
            html.contains(r#"src="/static/icons/trash.svg"#),
            "delete icon url"
        );
    }

    /// Filenames with URL-special chars (spaces, `#`, `?`, `&`, parens) must
    /// be percent-encoded in `/file/{name}` hrefs so the browser requests the
    /// right path instead of truncating at `#` / starting the query at `?` /
    /// splitting params at `&`. The visible confirm text stays human-readable.
    #[test]
    fn file_links_percent_encode_special_filenames() {
        let mut it = item(ItemStatus::Done, None);
        it.filename = Some("Video #1 (HQ) & more.webm".into());
        let html = render_card(&it);

        // href path segment is percent-encoded; no raw space / # / ? / &.
        assert!(
            html.contains(r#"href="/file/Video%20%231%20%28HQ%29%20%26%20more.webm?download=1"#)
        );
        assert!(html.contains(r#"href="/file/Video%20%231%20%28HQ%29%20%26%20more.webm?inline=1"#));
        assert!(
            !html.contains(r#"href="/file/Video #"#),
            "raw space/# in href"
        );

        // Confirm dialog stays human-readable (HTML-escaped, not %20).
        assert!(
            html.contains(r#"hx-confirm="Delete Video #1 (HQ) &#38; more.webm from the server?"#)
        );
    }

    /// Pending card surfaces a cancel button (not delete/open/download). The
    /// whole card is a stretched link to the details page.
    #[test]
    fn pending_card_overlay_has_cancel() {
        let html = render_card(&item(ItemStatus::Pending, None));
        assert!(html.contains(r#"title="cancel download"#));
        assert!(
            html.contains(r#"class="card-link" href="/item/7" target="_blank" rel="noopener""#),
            "stretched link opens details in a new tab"
        );
        assert!(!html.contains("/static/icons/info.svg"), "no inspect icon");
        assert_eq!(html.matches("<img").count(), 1, "cancel icon only");
        assert!(
            html.contains(r#"src="/static/icons/stop.svg"#),
            "cancel icon url"
        );
        // No done-state actions on a pending card.
        assert!(!html.contains(r#"title="download to this device"#));
    }

    /// A failed (or cancelled) card surfaces a retry button *and* a trash
    /// button that removes the item from the download history (POST
    /// /delete-item/:id -- the item is terminal, so the state file is dropped
    /// on the next persist). No file is touched (there is none).
    #[test]
    fn failed_card_overlay_has_retry_and_trash() {
        let mut it = item(ItemStatus::Failed, None);
        it.error = Some("Video unavailable".into());
        let html = render_card(&it);
        assert!(html.contains(r#"title="retry download"#));
        assert!(
            html.contains(r#"title="remove from history"#),
            "trash tooltip"
        );
        assert!(
            html.contains(r#"hx-post="/delete-item/7""#),
            "trash posts to delete-item"
        );
        // No confirm dialog for a history-only removal (no file to lose).
        assert!(
            !html.contains("hx-confirm"),
            "no confirm for history removal"
        );
        assert!(!html.contains(r#"title="delete from server"#));
        // retry + trash icons.
        assert_eq!(html.matches("<img").count(), 2, "retry + trash icons");
        assert!(html.contains(r#"src="/static/icons/retry.svg"#));
        assert!(html.contains(r#"src="/static/icons/trash.svg"#));
        // The captured error surfaces on the card.
        assert!(html.contains("Video unavailable"));
        // Card is still a stretched link to the details page (new tab).
        assert!(
            html.contains(r#"class="card-link" href="/item/7" target="_blank" rel="noopener""#)
        );
    }

    /// `StatusParts` produces the six banner-slot fragments for each state:
    /// idle (all empty, which hides the banner via CSS), queued (title line
    /// only), and active (thumb + title + bar + meta + cancel). `status_events`
    /// wraps them in the six targeted `status-*` events in slot order.
    #[test]
    fn status_parts_states() {
        // Idle: every slot empty (the empty `title` hides the banner).
        let idle = StatusParts::new(None, 0);
        assert_eq!(idle, StatusParts::default());
        let idle_evts = status_events(None, 0);
        assert_eq!(idle_evts.len(), 6);
        assert_eq!(idle_evts[0].name(), "status-thumb");
        assert_eq!(idle_evts[1].name(), "status-title");
        assert_eq!(idle_evts[5].name(), "status-cancel");
        for ev in &idle_evts {
            assert!(ev.data().is_empty(), "idle {:?} not empty", ev.name());
        }

        // Queued: only the title slot is populated ("Waiting… N queued");
        // the thumb / log / bar / meta / cancel slots stay empty so their
        // columns / lines collapse.
        let queued = StatusParts::new(None, 3);
        assert!(queued.thumb.is_empty());
        assert!(queued.title.contains("Waiting&hellip;"));
        assert!(queued.title.contains("3 queued"));
        assert!(queued.log.is_empty() && queued.bar.is_empty());
        assert!(queued.meta.is_empty() && queued.cancel.is_empty());

        // Active: every slot is populated; the thumb is the landed image, the
        // bar is the bigger progress bar, the meta carries percent + ETA, and
        // the cancel button targets the active item's id.
        let mut it = item(ItemStatus::Active, Some("t.jpg"));
        it.progress = Some(crate::state::Progress::default());
        let active = StatusParts::new(Some(&it), 2);
        assert!(active.thumb.contains("/thumb/t.jpg"), "thumbnail present");
        assert!(
            active.bar.contains("bn-bar") || active.bar.contains("<i"),
            "bar fill present"
        );
        assert!(active.meta.contains("0%"), "percent present");
        assert!(active.title.contains("Hello World"), "title present");
        assert!(active.title.contains("2 queued"), "queued badge present");
        assert!(
            active.cancel.contains("hx-post=\"/cancel/7\""),
            "cancel button targets the active item id"
        );
        // No log line yet -> log slot empty (collapses).
        assert!(active.log.is_empty());
    }

    /// Each card root carries a stable `id="card-<id>"` plus an
    /// `sse-swap="card-<id>"` + `hx-swap="outerHTML"`, so a per-card SSE
    /// event can replace exactly that card (and an empty payload remove it)
    /// without touching the rest of the grid. This is the contract the
    /// targeted-grid model (`Event::Card` / `Event::CardAdded`) depends on.
    #[test]
    fn card_root_has_stable_id_and_per_card_sse_swap() {
        let html = render_card(&item(ItemStatus::Active, None));
        assert!(
            html.contains(r#"id="card-7" sse-swap="card-7" hx-swap="outerHTML""#),
            "card root is its own targeted sse-swap element"
        );
        assert!(html.contains(r#"data-id="7""#), "data-id preserved");
    }

    /// `render_cards_count` produces the header count text, and is empty for
    /// an empty queue (so the `#cards-count` span collapses, matching the old
    /// full-grid render which omitted the count entirely when empty).
    #[test]
    fn cards_count_text() {
        assert_eq!(render_cards_count(0, 0), "");
        assert_eq!(render_cards_count(3, 1), "3 total, 1 pending");
        assert_eq!(render_cards_count(5, 0), "5 total, 0 pending");
    }

    /// The full-grid snapshot (`render_queue`, swapped into `#cards` on
    /// connect / lag-recovery) splits the header into a static title + a
    /// `#cards-count` sse-swap target, and the list into a `#cards-list`
    /// `card-added` prepend target -- and always renders the `.empty` hint
    /// (hidden via CSS when a card is present). Each card in the snapshot
    /// carries its own per-card listener so it picks up targeted updates after
    /// the re-render.
    #[test]
    fn queue_snapshot_has_targeted_targets_and_empty_hint() {
        use crate::state::Queue;
        let mut q = Queue::new();
        let _ = q.enqueue("https://example/v/1".into(), Some("A".into()), None);
        let html = render_queue(&q);
        assert!(
            html.contains(r#"id="cards-count" class="cards-count" sse-swap="cards-count""#),
            "count is its own sse-swap target"
        );
        assert!(
            html.contains(
                r#"id="cards-list" class="cards-list" sse-swap="card-added" hx-swap="afterbegin""#
            ),
            "list is the card-added prepend target"
        );
        assert!(
            html.contains(r#"sse-swap="card-"#),
            "snapshot cards carry per-card listeners"
        );
        assert!(html.contains("no videos yet"), "empty hint always present");
        assert!(
            html.contains("1 total, 1 pending"),
            "count text present when non-empty"
        );

        // Empty queue: count text collapses, empty hint present.
        let empty = render_queue(&Queue::new());
        assert!(!empty.contains("total"), "no count text when empty");
        assert!(empty.contains("no videos yet"));
    }
}

#[cfg(test)]
mod item_page_tests {
    use super::*;
    use crate::state::{ItemStatus, QueueItem};

    fn item(status: ItemStatus) -> QueueItem {
        let mut it = QueueItem::new(42, "https://example/watch?v=x".into());
        it.title = Some("Hello World".into());
        it.duration = Some(123.0);
        it.status = status;
        it.thumbnail = Some("thumb-abc.jpg".into());
        it
    }

    /// A finished video's details page is a full standalone document: it
    /// loads htmx + the stylesheet, shows the full thumbnail, surfaces the
    /// View / Download / Delete big text buttons with percent-encoded file
    /// links, lists the metadata, and includes the (non-polling) log body.
    #[test]
    fn done_item_page_has_full_doc_and_actions() {
        let mut it = item(ItemStatus::Done);
        it.filename = Some("Video #1.webm".into());
        it.logs.push("[download] 100%".into());
        let html = render_item_page(&it);

        assert!(html.starts_with("<!DOCTYPE html>"), "full document");
        assert!(html.contains(r#"href="/static/app.css""#), "stylesheet");
        assert!(
            html.contains(r#"<script src="/static/htmx.org-2.0.4.js">"#),
            "htmx"
        );
        // Full thumbnail uses the cached thumbnail route.
        assert!(html.contains(r#"<img src="/thumb/thumb-abc.jpg""#));
        // Big text buttons for a finished file, with percent-encoded links.
        assert!(html.contains(r#"class="big-btn view"#));
        assert!(html.contains(r#"href="/file/Video%20%231.webm?inline=1"#));
        assert!(html.contains(r#"class="big-btn download"#));
        assert!(html.contains(r#"href="/file/Video%20%231.webm?download=1"#));
        assert!(html.contains(r#"class="big-btn delete"#));
        assert!(html.contains(r#"hx-post="/delete-item/42""#));
        // Metadata + log body present.
        assert!(html.contains("Hello World"), "title");
        assert!(html.contains("https://example/watch?v=x"), "url");
        assert!(html.contains("Video #1.webm"), "filename");
        assert!(html.contains(r#"id="item-log-lines"#), "log body");
        assert!(html.contains("[download] 100%"), "log line");
        // Done items don't poll (no more output expected).
        assert!(
            !html.contains("hx-trigger=\"every 2s\""),
            "no polling when done"
        );
    }

    /// An in-flight item's page shows Cancel (not View/Download/Delete) and
    /// wires the log body to poll /logs/:id?lines=1 every 2s.
    #[test]
    fn active_item_page_polls_logs_and_shows_cancel() {
        let it = item(ItemStatus::Active);
        let html = render_item_page(&it);
        assert!(html.contains(r#"class="big-btn cancel"#));
        assert!(html.contains(r#"hx-post="/cancel/42""#));
        assert!(!html.contains(r#"class="big-btn view"#));
        assert!(
            html.contains(r#"hx-get="/logs/42?lines=1""#),
            "poll endpoint"
        );
        assert!(html.contains(r#"hx-trigger="every 2s""#), "polls every 2s");
    }

    /// A failed terminal item shows Retry + Delete and the captured error.
    #[test]
    fn failed_item_page_shows_retry_and_error() {
        let mut it = item(ItemStatus::Failed);
        it.error = Some("Video unavailable".into());
        let html = render_item_page(&it);
        assert!(html.contains(r#"class="big-btn retry"#));
        assert!(html.contains(r#"class="big-btn delete"#));
        assert!(html.contains("Video unavailable"), "error surfaced");
        assert!(!html.contains(r#"class="big-btn view"#));
    }

    /// The "gone" page is a valid document with a back link (not a bare 404).
    #[test]
    fn gone_page_is_a_valid_document() {
        let html = render_item_gone();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains(r#"<a class="ip-back" href="/">"#));
        assert!(html.contains("no longer in the queue"));
    }
}
