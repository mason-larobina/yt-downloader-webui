//! Render server-side HTML fragments for SSE events.
use std::path::Path;

use crate::parse::FlatEntry;
use crate::state::{ItemStatus, Queue, QueueItem};
use crate::library::LibraryFile;

/// JSON shape embedded in each approval checkbox `value`, so POST /approve can
/// reconstruct per-video items (with titles) without any server-side stash.
/// The whole blob is HTML-escaped into the attribute; the browser decodes it
/// back to this JSON on form submit.
#[derive(serde::Serialize)]
struct ApprovalEntry<'a> {
    url: &'a str,
    title: Option<&'a str>,
    duration: Option<f64>,
    /// Best-thumbnail URL harvested by the probe; POST /approve fetches it into
    /// the cache and attaches the resulting filename to the enqueued item.
    thumbnail: Option<&'a str>,
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

/// Percent-encode a filename for safe use as a single URL path segment in an
/// `href`. Encodes everything except RFC 3986 unreserved chars
/// (`A-Za-z0-9-._~`); notably spaces -> `%20`, and `#` / `?` / `&` / `(` /
/// `)` / non-ASCII are encoded so they can't be misread as fragment / query /
/// separator boundaries. The result contains no HTML-special characters,
/// so it is safe to drop straight into a double-quoted attribute.
pub fn url_encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
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

/// Render the `#status` fragment: a spotify-like floating banner at the
/// bottom of the viewport showing the active download's thumbnail, title,
/// latest yt-dlp log line, a (bigger) progress bar, duration/ETA and
/// speed/bytes, plus a live count of pending items. When nothing is active
/// but items are queued, it shows a compact "N queued — waiting" line; when
/// the queue is fully idle it renders an empty (hidden) banner.
///
/// `pending` is the number of Pending items (drives the live queued count).
pub fn render_status(active: Option<&QueueItem>, pending: usize) -> String {
    // Fully idle: hide the banner.
    if active.is_none() && pending == 0 {
        return r#"<div id="status" class="banner idle" sse-swap="status" hx-swap="outerHTML"></div>"#
            .to_string();
    }

    // Nothing active yet, but items are waiting.
    if active.is_none() {
        return format!(
            r#"<div id="status" class="banner queued" sse-swap="status" hx-swap="outerHTML"><div class="bn-body"><div class="bn-top"><span class="bn-title">Waiting&hellip;</span><span class="bn-queued">{n} queued</span></div></div></div>"#,
            n = pending
        );
    }

    let item = active.unwrap();
    let label = esc(item.label());
    let p = item.progress.as_ref();
    let percent = p.and_then(|p| p.percent()).unwrap_or(0.0);
    let width = percent.clamp(0.0, 100.0);
    let speed = human_speed(p.and_then(|p| p.speed));
    let eta = human_eta(p.and_then(|p| p.eta));
    let bytes = p.and_then(|p| {
        let dl = p.downloaded_bytes? as u64;
        let tot = p
            .total_bytes
            .or(p.total_bytes_estimate)
            .map(|x| x as u64);
        Some(match tot {
            Some(tot) if tot > 0 => format!(
                "{} / {}",
                human_bytes(dl),
                human_bytes(tot)
            ),
            _ => human_bytes(dl),
        })
    });
    let dur = human_duration(item.duration);

    let thumb_html = match &item.thumbnail {
        Some(name) => format!(
            r##"<img class="bn-img" src="/thumb/{name}" alt="" loading="lazy">"##,
            name = esc(name)
        ),
        None => r#"<div class="bn-img bn-img-placeholder"></div>"#.to_string(),
    };

    // Latest yt-dlp log line as a subtitle.
    let log_html = match item.logs.last() {
        Some(l) => format!(
            r#"<div class="bn-log">{}</div>"#,
            esc(l.trim_end_matches('\n'))
        ),
        None => String::new(),
    };

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

    let queued_html = if pending > 0 {
        format!(r#"<span class="bn-queued">{n} queued</span>"#, n = pending)
    } else {
        String::new()
    };

    format!(
        r#"<div id="status" class="banner active" sse-swap="status" hx-swap="outerHTML">{thumb}<div class="bn-body"><div class="bn-top"><span class="bn-title">{label}</span>{queued}</div>{log}<div class="bn-bar"><i style="width:{w:.0}%"></i></div><div class="bn-meta"><span class="bn-left">{left}</span><span class="bn-pct">{pct:.0}%</span><span class="bn-right">{right}</span></div></div></div>"#,
        thumb = thumb_html,
        label = label,
        queued = queued_html,
        log = log_html,
        w = width,
        pct = percent,
        left = left_bits.join(" &middot; "),
        right = right_bits.join(" &middot; "),
    )
}

// ----------------------------- #cards --------------------------------------

/// Render the cards-pane inner fragment (swapped into `#cards` via the
/// `queue` SSE event). Cards are rendered newest-first (most recently
/// enqueued at the top) so the latest activity is visible without scrolling.
/// Each card is a large thumbnail with title + progress; an overlay on the
/// thumbnail's top-right exposes contextual actions (download/open/delete/
/// logs for done items; cancel/logs for active/pending; retry/logs for
/// failed/cancelled).
pub fn render_queue(queue: &Queue) -> String {
    let pending = queue
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Pending)
        .count();
    let terminal = queue
        .items
        .iter()
        .filter(|i| i.status.is_terminal())
        .count();
    let total = queue.items.len();

    let clear_btn = if terminal > 0 {
        format!(
            r##"<button class="clear" hx-post="/clear" hx-target="#ack" hx-swap="innerHTML">clear {n}</button>"##,
            n = terminal
        )
    } else {
        String::new()
    };

    if queue.items.is_empty() {
        return r##"<div class="cards-head"><span class="cards-title">downloads</span></div><div class="cards-list"><div class="empty">no videos yet &mdash; paste a URL</div></div>"##
            .to_string();
    }

    let head = format!(
        r##"<div class="cards-head"><span class="cards-title">downloads</span><span class="cards-count">{total} total, {pending} pending</span>{clear}</div>"##,
        total = total,
        pending = pending,
        clear = clear_btn,
    );

    let mut cards = String::new();
    // Newest first: iterate the queue (FIFO by enqueue time) in reverse.
    for item in queue.items.iter().rev() {
        cards.push_str(&render_card(item));
    }

    format!(r#"{head}<div class="cards-list">{cards}</div>"#, cards = cards)
}

/// Render one video card. The card shows the thumbnail, a status badge, the
/// title/duration, and (for failures) the error. Per-card progress is not
/// shown here -- live progress lives in the floating bottom banner instead.
fn render_card(item: &QueueItem) -> String {
    let status = item.status.as_str();
    let label = esc(item.label());
    let dur = human_duration(item.duration);

    let thumb_html = match &item.thumbnail {
        Some(name) => format!(
            r##"<img class="card-img" src="/thumb/{name}" alt="" loading="lazy">"##,
            name = esc(name)
        ),
        None => r#"<div class="card-img card-img-placeholder"></div>"#.to_string(),
    };

    let overlay = render_card_overlay(item);

    let error_html = match (&item.status, &item.error) {
        (ItemStatus::Failed, Some(e)) => {
            format!(r#"<div class="card-err">{e}</div>"#, e = esc(e))
        }
        _ => String::new(),
    };

    let sub_html = if dur.is_empty() {
        format!(r#"<div class="card-sub">{status}</div>"#)
    } else {
        format!(r#"<div class="card-sub"><span class="dur">{dur}</span> &middot; {status}</div>"#)
    };

    format!(
        r##"<div class="card {status}" data-id="{id}"><div class="card-thumb">{thumb}<span class="card-badge {status}">{status}</span><div class="card-overlay">{overlay}</div></div><div class="card-meta"><div class="card-title">{label}</div>{sub}</div>{err}</div>"##,
        status = status,
        id = item.id,
        thumb = thumb_html,
        overlay = overlay,
        label = label,
        sub = sub_html,
        err = error_html,
    )
}

/// Render the top-right thumbnail overlay buttons, context-aware by status.
fn render_card_overlay(item: &QueueItem) -> String {
    let id = item.id;
    let logs_btn = format!(
        r##"<button class="ov-btn" hx-get="/logs/{id}" hx-target="#logs-pane" hx-swap="innerHTML" title="show yt-dlp logs">logs</button>"##,
        id = id
    );
    let actions: String = match item.status {
        ItemStatus::Pending | ItemStatus::Active => format!(
            r##"<button class="ov-btn ov-warn" hx-post="/cancel/{id}" hx-target="#ack" hx-swap="innerHTML">cancel</button>"##,
            id = id
        ),
        ItemStatus::Failed | ItemStatus::Cancelled => format!(
            r##"<button class="ov-btn ov-ok" hx-post="/retry/{id}" hx-target="#ack" hx-swap="innerHTML">retry</button>"##,
            id = id
        ),
        ItemStatus::Done => match &item.filename {
            Some(name) => {
                let n = url_encode_path(name);
                let disp = esc(name);
                format!(
                    r##"<a class="ov-btn" href="/file/{n}?download=1" title="download to this device">download</a><a class="ov-btn" href="/file/{n}?inline=1" target="_blank" rel="noopener" title="open/preview">open</a><button class="ov-btn ov-warn" hx-post="/delete-item/{id}" hx-target="#ack" hx-swap="innerHTML" hx-confirm="Delete {disp} from the server?">delete</button>"##,
                    n = n,
                    disp = disp,
                    id = id
                )
            }
            None => String::new(),
        },
    };
    format!(r#"{actions}{logs_btn}"#)
}

// ----------------------------- #logs-pane ----------------------------------

/// Render the full logs-pane fragment (swapped into `#logs-pane` by GET
/// /logs/:id). Comprises a header (label + close + status) and a scrollable
/// body whose inner `#lp-lines` polls GET /logs/:id?lines=1 every 2s while the
/// item is still in flight, so the pane auto-updates without resetting the
/// user's scroll position (the scroll container itself is never swapped).
pub fn render_logs_pane(item: &QueueItem) -> String {
    let id = item.id;
    let label = esc(item.label());
    let status = item.status.as_str();
    let lines = render_log_lines(&item.logs);
    // Only poll while the download may still produce output. Terminal items
    // render a static snapshot.
    let poll = if matches!(item.status, ItemStatus::Pending | ItemStatus::Active) {
        format!(
            r##"hx-get="/logs/{id}?lines=1" hx-trigger="every 2s" hx-target="this" hx-swap="innerHTML""##,
            id = id
        )
    } else {
        String::new()
    };
    format!(
        r##"<div class="lp-head"><span class="lp-title">logs</span><span class="lp-label">{label}</span><span class="lp-status {status}">{status}</span><button class="lp-close" type="button" onclick="closeLogs()">close</button></div><div id="lp-body" class="lp-body"><div id="lp-lines" class="lp-lines" {poll}>{lines}</div></div>"##,
        label = label,
        status = status,
        poll = poll,
        lines = lines,
    )
}

/// Render the inner log-line divs for an item (the body of `#lp-lines`).
pub fn render_log_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        return r#"<div class="lp-empty-lines">(no output yet)</div>"#.to_string();
    }
    let mut out = String::new();
    for l in lines {
        out.push_str(&render_log_line(l));
    }
    out
}

// ----------------------------- #log ----------------------------------------

/// Render one escaped log line as a fragment to append.
pub fn render_log_line(line: &str) -> String {
    format!(
        r#"<div class="logline">{}</div>"#,
        esc(line.trim_end_matches('\n'))
    )
}

// ----------------------------- #library ------------------------------------

/// Render the full `#library` fragment from a scanned file list.
pub fn render_library(files: &[LibraryFile]) -> String {
    let mut rows = String::new();
    for f in files {
        rows.push_str(&render_library_row(f));
    }
    if files.is_empty() {
        return r#"<div id="library" class="library"><div class="empty">no files</div></div>"#
            .to_string();
    }
    format!(
        r##"<div id="library" class="library"><div class="lhead">library ({n} files) <button class="refresh" hx-get="/library" hx-target="#library" hx-swap="outerHTML">refresh</button></div>{rows}</div>"##,
        n = files.len(),
        rows = rows,
    )
}

fn render_library_row(f: &LibraryFile) -> String {
    let name = esc(&f.name);
    let name_url = url_encode_path(&f.name);
    let size = human_bytes(f.size);
    let mtime = f
        .mtime
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_default();
    format!(
        r##"<div class="lrow"><span class="name">{name}</span> <span class="size">{size}</span> <span class="mtime">{mtime}</span> <a class="open" href="/file/{name_url}?inline=1">open</a> <a class="dl" href="/file/{name_url}?download=1">download</a> <button class="del" hx-post="/delete/{name_url}" hx-target="#ack" hx-swap="innerHTML" hx-confirm="Delete {name}?">delete</button></div>"##,
        name = name,
        name_url = name_url,
        size = size,
        mtime = mtime,
    )
}

// ----------------------------- #approval ----------------------------------

/// Render the approval list for a probed playlist: a form whose checkboxes
/// (checked by default) carry each entry's url/title/duration as an
/// HTML-escaped JSON `value`. POST /approve deserialises the checked values
/// and enqueues one per-video `Video` item each. `note` is an optional line
/// shown above the list (e.g. "1 single-video URL queued directly" or a
/// per-URL probe error).
///
/// Never persisted: this fragment is a transient request-handler response.
/// Only the approved per-video items that result from POST /approve reach the
/// queue (and thus the state dir).
pub fn render_approval(title: Option<&str>, entries: &[FlatEntry], note: Option<&str>) -> String {
    let mut rows = String::new();
    for (i, e) in entries.iter().enumerate() {
        let url = e.url.as_deref().unwrap_or("");
        let blob = serde_json::to_string(&ApprovalEntry {
            url,
            title: e.title.as_deref(),
            duration: e.duration,
            thumbnail: e.thumbnail.as_deref(),
        })
        .unwrap_or_default();
        let value = esc(&blob);
        let label = esc(e.title.as_deref().unwrap_or(url));
        let dur = human_duration(e.duration);
        let idx = i + 1;
        let dur_html = if dur.is_empty() {
            String::new()
        } else {
            format!(r#" <span class="dur">{dur}</span>"#)
        };
        rows.push_str(&format!(
            r##"<div class="arow"><label><input type="checkbox" name="entry" value="{value}" checked> <span class="idx">{idx}.</span> <span class="title">{label}</span>{dur_html}</label></div>"##,
            value = value,
            idx = idx,
            label = label,
            dur_html = dur_html,
        ));
    }

    let n = entries.len();
    let head = match title {
        Some(t) => format!(
            r##"<div class="ahead">playlist: {t} ({n} videos)</div>"##,
            t = esc(t),
            n = n
        ),
        None => format!(r##"<div class="ahead">playlist ({n} videos)</div>"##, n = n),
    };
    let note_html = match note {
        Some(s) if !s.is_empty() => format!(r##"<div class="anote">{}</div>"##, esc(s)),
        _ => String::new(),
    };

    format!(
        r##"<form class="approvelist" hx-post="/approve" hx-target="#approve" hx-swap="innerHTML">{head}{note_html}<div class="arows">{rows}</div><div class="actions"><button type="submit">Download selected (<span class="sel-count">{n}</span>)</button></div></form>"##,
        head = head,
        note_html = note_html,
        rows = rows,
        n = n,
    )
}

// ----------------------------- snapshots -----------------------------------

/// Build the queue + log snapshot used by SSE on connect: returns the queue
/// fragment, then each log-line fragment (already-rendered).
pub fn snapshot_log_lines(lines: &[&String]) -> Vec<String> {
    lines.iter().map(|l| render_log_line(l)).collect()
}

/// Convenience: render library for a path scan (used by both GET /library and
/// the library event). Returns the fragment string.
pub fn render_library_scan(dir: &Path) -> String {
    match crate::library::scan(dir) {
        Ok(files) => render_library(&files),
        Err(_) => r#"<div id="library" class="library"><div class="err">failed to scan directory</div></div>"#.to_string(),
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use crate::parse::FlatEntry;

    /// The checkbox `value` is an HTML-escaped JSON blob that POST /approve
    /// must be able to deserialise back into `{url,title,duration}`. This
    /// pins the contract between `render_approval` and `server::ApproveEntry`.
    #[derive(serde::Deserialize)]
    struct ApproveEntry {
        url: String,
        title: Option<String>,
        duration: Option<f64>,
        thumbnail: Option<String>,
    }

    #[test]
    fn approval_checkbox_values_round_trip() {
        let entries = vec![
            FlatEntry {
                _type: Some("url".into()),
                url: Some("https://www.youtube.com/watch?v=aaa".into()),
                title: Some("First & <second> \"quoted\"".into()),
                duration: Some(3623.0),
                thumbnail: Some("https://i.ytimg.com/vi/aaa/hqdefault.jpg".into()),
                ..Default::default()
            },
            FlatEntry {
                _type: Some("url".into()),
                url: Some("https://www.youtube.com/watch?v=bbb".into()),
                title: None,
                duration: None,
                thumbnail: None,
                ..Default::default()
            },
        ];
        let html = render_approval(Some("Chill"), &entries, Some("1 single-video URL queued directly"));

        // Form posts to /approve into #approve.
        assert!(html.contains(r##"hx-post="/approve""##));
        assert!(html.contains(r##"hx-target="#approve""##));
        assert!(html.contains("playlist: Chill (2 videos)"));
        assert!(html.contains("1 single-video URL queued directly"));

        // Extract every checkbox value, HTML-unescape (as a browser would),
        // and confirm each deserialises with title/duration intact -- including
        // the entry whose title contains HTML-special chars (& < > ").
        //
        // `esc` turns every `"` in the JSON into `&quot;`, so the attribute
        // value contains no raw `"`; it runs from `value="` to the next `"`.
        let needle = r#"name="entry" value=""#;
        let values: Vec<String> = html
            .match_indices(needle)
            .map(|(i, _)| {
                let start = i + needle.len();
                let rest = &html[start..];
                let end = rest.find('"').unwrap_or(rest.len());
                rest[..end].to_string()
            })
            .collect();
        assert_eq!(values.len(), 2, "expected 2 checkboxes, got {values:?}");

        // Browser decodes HTML entities in the attribute value before submit.
        fn unescape(s: &str) -> String {
            s.replace("&quot;", "\"")
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&#39;", "'")
        }
        let first: ApproveEntry =
            serde_json::from_str(&unescape(&values[0])).expect("first value decodes");
        assert_eq!(first.url, "https://www.youtube.com/watch?v=aaa");
        assert_eq!(first.title.as_deref(), Some(r#"First & <second> "quoted""#));
        assert_eq!(first.duration, Some(3623.0));
        assert_eq!(
            first.thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/aaa/hqdefault.jpg")
        );

        let second: ApproveEntry =
            serde_json::from_str(&unescape(&values[1])).expect("second value decodes");
        assert_eq!(second.url, "https://www.youtube.com/watch?v=bbb");
        assert!(second.title.is_none());
        assert!(second.duration.is_none());
        assert!(second.thumbnail.is_none());
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
        let thumb_start = html.find("<div class=\"card-thumb\">").expect("card-thumb wrapper");
        let thumb_end = html[thumb_start..]
            .find("</div>")
            .map(|e| thumb_start + e)
            .expect("card-thumb close");
        let thumb = &html[thumb_start..thumb_end];
        assert!(thumb.contains("<img class=\"card-img\""), "img inside thumb");
        assert!(thumb.contains("card-overlay"), "overlay inside thumb");
        assert!(thumb.contains("card-badge"), "badge inside thumb");
    }

    /// The done card surfaces download/open/delete overlay buttons.
    #[test]
    fn done_card_overlay_has_actions() {
        let mut it = item(ItemStatus::Done, None);
        it.filename = Some("video.webm".into());
        let html = render_card(&it);
        assert!(html.contains(">download<"));
        assert!(html.contains(">open<"));
        assert!(html.contains(">delete<"));
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
        assert!(html.contains(r#"href="/file/Video%20%231%20%28HQ%29%20%26%20more.webm?download=1"#));
        assert!(html.contains(r#"href="/file/Video%20%231%20%28HQ%29%20%26%20more.webm?inline=1"#));
        assert!(!html.contains(r#"href="/file/Video #"#), "raw space/# in href");

        // Confirm dialog stays human-readable (HTML-escaped, not %20).
        assert!(html.contains(r#"hx-confirm="Delete Video #1 (HQ) &amp; more.webm from the server?"#));
    }

    /// Pending card surfaces a cancel button (not delete/open/download).
    #[test]
    fn pending_card_overlay_has_cancel() {
        let html = render_card(&item(ItemStatus::Pending, None));
        assert!(html.contains(">cancel<"));
        assert!(!html.contains(">download<"));
    }

    /// render_status idle banner is hidden; queued banner shows the count;
    /// active banner shows the bigger bar + thumbnail + queued count.
    #[test]
    fn status_banner_states() {
        let idle = render_status(None, 0);
        assert!(idle.contains("banner idle"));

        let queued = render_status(None, 3);
        assert!(queued.contains("banner queued"));
        assert!(queued.contains("3 queued"));

        let mut it = item(ItemStatus::Active, Some("t.jpg"));
        it.progress = Some(crate::state::Progress::default());
        let active = render_status(Some(&it), 2);
        assert!(active.contains("banner active"));
        assert!(active.contains("bn-bar"), "bigger progress bar present");
        assert!(active.contains("/thumb/t.jpg"), "thumbnail present");
        assert!(active.contains("2 queued"));
        assert!(active.contains("Hello World"), "title present");
    }
}
