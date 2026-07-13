//! Render server-side HTML fragments for SSE events.
use std::path::Path;

use crate::parse::FlatEntry;
use crate::state::{ItemStatus, Queue, QueueItem};
use crate::library::LibraryFile;

/// JSON shape embedded in each probe-result card checkbox `value`, so POST
/// /confirm can reconstruct per-video items (with titles + thumbnail URLs)
/// without any server-side stash. The whole blob is HTML-escaped into the
/// attribute; the browser decodes it back to this JSON on form submit.
#[derive(serde::Serialize)]
struct ApprovalEntry<'a> {
    url: &'a str,
    title: Option<&'a str>,
    duration: Option<f64>,
    /// Best-thumbnail URL harvested by the probe; POST /confirm fetches it into
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

/// An overlay-button icon: a cached `<img>` served from `/static/icons`, so
/// the SVG markup is fetched once per icon and reused across every card
/// instead of being inlined into each card's HTML. Sizing is handled by
/// `.ov-btn img` in app.css; the explicit `width`/`height` guard against
/// layout shift before the (tiny) image loads. `alt` is empty because the
/// surrounding button already exposes its action via `title`.
fn icon(name: &str) -> &'static str {
    match name {
        "download" => r#"<img class="ov-icon" src="/static/icons/download.svg" alt="" width="16" height="16" loading="lazy">"#,
        "open" => r#"<img class="ov-icon" src="/static/icons/play.svg" alt="" width="16" height="16" loading="lazy">"#,
        "delete" => r#"<img class="ov-icon" src="/static/icons/trash.svg" alt="" width="16" height="16" loading="lazy">"#,
        "logs" => r#"<img class="ov-icon" src="/static/icons/logs.svg" alt="" width="16" height="16" loading="lazy">"#,
        "cancel" => r#"<img class="ov-icon" src="/static/icons/stop.svg" alt="" width="16" height="16" loading="lazy">"#,
        "retry" => r#"<img class="ov-icon" src="/static/icons/retry.svg" alt="" width="16" height="16" loading="lazy">"#,
        _ => "",
    }
}

/// Render the top-right thumbnail overlay buttons, context-aware by status.
/// Each button shows a coloured icon served from `/static/icons/{name}` (a
/// single cached request reused across every card, rather than inlining the
/// SVG markup per card). The icon's `title` tooltip spells out the action in
/// words for accessibility; the icons carry the semantic colour, so the
/// buttons use a neutral border rather than per-action coloured borders.
fn render_card_overlay(item: &QueueItem) -> String {
    let id = item.id;
    let logs_btn = format!(
        r##"<button class="ov-btn" hx-get="/logs/{id}" hx-target="#logs-pane" hx-swap="innerHTML" title="show yt-dlp logs">{icon}</button>"##,
        id = id,
        icon = icon("logs"),
    );
    let actions: String = match item.status {
        ItemStatus::Pending | ItemStatus::Active => format!(
            r##"<button class="ov-btn" hx-post="/cancel/{id}" hx-target="#ack" hx-swap="innerHTML" title="cancel download">{icon}</button>"##,
            id = id,
            icon = icon("cancel"),
        ),
        ItemStatus::Failed | ItemStatus::Cancelled => format!(
            r##"<button class="ov-btn" hx-post="/retry/{id}" hx-target="#ack" hx-swap="innerHTML" title="retry download">{icon}</button>"##,
            id = id,
            icon = icon("retry"),
        ),
        ItemStatus::Done => match &item.filename {
            Some(name) => {
                let n = url_encode_path(name);
                let disp = esc(name);
                format!(
                    r##"<a class="ov-btn" href="/file/{n}?download=1" title="download to this device">{dl}</a><a class="ov-btn" href="/file/{n}?inline=1" target="_blank" rel="noopener" title="open/preview">{op}</a><button class="ov-btn" hx-post="/delete-item/{id}" hx-target="#ack" hx-swap="innerHTML" hx-confirm="Delete {disp} from the server?" title="delete from server">{del}</button>"##,
                    n = n,
                    disp = disp,
                    id = id,
                    dl = icon("download"),
                    op = icon("open"),
                    del = icon("delete"),
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

// ----------------------------- header / probe ------------------------------

/// Render the normal header input form (a single URL text field + Add
/// button). Returned by GET /header and POST /confirm (to restore the header
/// after a confirm/discard), and by POST /download when the submitted URL is
/// empty. `error` is an optional inline message shown beneath the field
/// (e.g. "paste a URL" or "select at least one video").
///
/// The form POSTs to /download, swapping the response (a probe-area shell)
/// into `#header-input` -- i.e. on submit the input is replaced by the
/// pending probe result area.
pub fn render_header_input(error: Option<&str>) -> String {
    let err_html = match error {
        Some(m) if !m.is_empty() => {
            format!(r#"<span class="header-err">{}</span>"#, esc(m))
        }
        _ => String::new(),
    };
    format!(
        r##"<form class="submit" hx-post="/download" hx-target="#header-input" hx-swap="innerHTML" hx-disabled-elt="#dl-btn"><input type="text" name="url" placeholder="paste a URL" autocomplete="off" autofocus><button type="submit" id="dl-btn">Add</button></form>{err}"##,
        err = err_html,
    )
}

/// Render the pending probe result area: a shell with its own SSE connection
/// (`sse-connect="/probe?url=…"`) that streams probe log lines into
/// `#probe-stream` (appended) and swaps the final result cards into
/// `#probe-cards` on the `result` event, then closes the stream
/// (`sse-close="result"`). A `cancel` button restores the header input
/// immediately (dropping the probe stream kills the yt-dlp probe process via
/// `kill_on_drop`).
///
/// The probe URL is percent-encoded for the query string with
/// [`url_encode_path`] (encodes everything except RFC 3986 unreserved chars,
/// so `&`, `=`, `#`, `?`, `"` etc. cannot break out of the `url=` param or
/// the double-quoted attribute).
pub fn render_probe_area(url: &str) -> String {
    let enc = url_encode_path(url);
    format!(
        r##"<div id="probe-area" class="probe-area" sse-connect="/probe?url={enc}" sse-close="result"><div class="probe-head"><span class="probe-status">probing&hellip;</span><button type="button" class="probe-cancel" hx-get="/header" hx-target="#header-input" hx-swap="innerHTML">cancel</button></div><div id="probe-stream" class="probe-stream" sse-swap="log" hx-swap="beforeend"></div><div id="probe-cards" class="probe-cards" sse-swap="result" hx-swap="innerHTML"></div></div>"##,
        enc = enc,
    )
}

/// Render the probe result cards (swapped into `#probe-cards` by the `result`
/// SSE event). On success this is a form of one or more cards -- each a
/// thumbnail + title + a checkbox (checked by default) whose `value` carries
/// the entry's `{url,title,duration,thumbnail}` as HTML-escaped JSON -- plus a
/// single Confirm / Cancel pair. Confirm (POST /confirm) enqueues the checked
/// entries and restores the header; Cancel restores the header without
/// enqueuing. On failure (no entries, no single video) an error line + Done
/// button is shown instead.
///
/// `submitted_url` is the original URL the user pasted; for a single-video
/// probe the entry's downloadable URL *is* the submitted URL (the probe
/// dict's `url` is a media URL, not a watch URL), so it is carried in the
/// card's checkbox value rather than the dict's `url`.
pub fn render_probe_result(
    submitted_url: &str,
    entries: &[FlatEntry],
    single: Option<&FlatEntry>,
    error: Option<&str>,
) -> String {
    // Build the list of (url, title, duration, thumbnail) to confirm.
    let cards: Vec<ApprovalEntry> = if !entries.is_empty() {
        entries
            .iter()
            .map(|e| ApprovalEntry {
                url: e.url.as_deref().unwrap_or(""),
                title: e.title.as_deref(),
                duration: e.duration,
                thumbnail: e.thumbnail.as_deref(),
            })
            .collect()
    } else if let Some(sv) = single {
        vec![ApprovalEntry {
            url: submitted_url,
            title: sv.title.as_deref(),
            duration: sv.duration,
            thumbnail: sv.thumbnail.as_deref(),
        }]
    } else {
        let msg = error
            .map(str::to_string)
            .unwrap_or_else(|| "no videos extracted".to_string());
        return format!(
            r##"<div class="probe-error"><span class="err">{msg}</span><button type="button" class="probe-done" hx-get="/header" hx-target="#header-input" hx-swap="innerHTML">Done</button></div>"##,
            msg = esc(&msg),
        );
    };

    let mut rows = String::new();
    for (i, e) in cards.iter().enumerate() {
        let blob = serde_json::to_string(e).unwrap_or_default();
        let value = esc(&blob);
        let label = esc(e.title.unwrap_or(e.url));
        let dur = human_duration(e.duration);
        let idx = i + 1;
        let thumb_html = match e.thumbnail {
            Some(t) => format!(
                r##"<img class="probe-thumb" src="{t}" alt="" loading="lazy" referrerpolicy="no-referrer">"##,
                t = esc(t),
            ),
            None => r#"<div class="probe-thumb probe-thumb-placeholder"></div>"#.to_string(),
        };
        let dur_html = if dur.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="probe-card-sub"><span class="dur">{dur}</span></div>"#)
        };
        rows.push_str(&format!(
            r##"<label class="probe-card"><input type="checkbox" name="entry" value="{value}" checked>{thumb}<div class="probe-card-meta"><div class="probe-card-title">{idx}. {label}</div>{dur_html}</div></label>"##,
            value = value,
            thumb = thumb_html,
            idx = idx,
            label = label,
            dur_html = dur_html,
        ));
    }

    let n = cards.len();
    format!(
        r##"<form class="probe-form" hx-post="/confirm" hx-target="#header-input" hx-swap="innerHTML"><div class="probe-cards-list">{rows}</div><div class="probe-actions"><button type="button" class="probe-logs-toggle" onclick="toggleProbeLogs()">show logs</button><span class="probe-sel-actions"><button type="button" onclick="probeSelectAll()">select all</button><button type="button" onclick="probeSelectNone()">deselect all</button></span><button type="submit">Confirm (<span class="sel-count">{n}</span>)</button><button type="button" hx-get="/header" hx-target="#header-input" hx-swap="innerHTML">Cancel</button></div></form>"##,
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
mod probe_result_tests {
    use super::*;
    use crate::parse::FlatEntry;

    /// The checkbox `value` is an HTML-escaped JSON blob that POST /confirm
    /// must be able to deserialise back into `{url,title,duration,thumbnail}`.
    /// This pins the contract between `render_probe_result` and
    /// `server::ApproveEntry`.
    #[derive(serde::Deserialize)]
    struct ApproveEntry {
        url: String,
        title: Option<String>,
        duration: Option<f64>,
        thumbnail: Option<String>,
    }

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
        s.replace("&quot;", "\"")
            .replace("&amp;", "&")
            .replace("&lt;", "<")
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

        assert!(html.contains(r##"hx-post="/confirm""##), "posts to /confirm");
        assert!(html.contains(r##"hx-target="#header-input""##));
        assert!(html.contains(r##"hx-get="/header""##), "cancel restores header");
        assert!(html.contains("Confirm ("));

        let values = checkbox_values(&html);
        assert_eq!(values.len(), 2, "expected 2 cards, got {values:?}");

        let first: ApproveEntry =
            serde_json::from_str(&unescape(&values[0])).expect("first value decodes");
        assert_eq!(first.url, "https://www.youtube.com/watch?v=aaa");
        assert_eq!(first.title.as_deref(), Some("First & <second> \"quoted\""));
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
        let e: ApproveEntry =
            serde_json::from_str(&unescape(&values[0])).expect("decodes");
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
        assert!(html.contains("https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Daaa%26list%3DPL1%26t%3D2"), "url encoded");
        assert!(html.contains("sse-swap=\"log\""), "sse-swap log");
        assert!(html.contains("hx-swap=\"beforeend\""), "log lines append");
        assert!(html.contains("sse-swap=\"result\""), "sse-swap result");
        assert!(html.contains("sse-close=\"result\""), "stream closes on result");
        assert!(html.contains("hx-get=\"/header\""), "cancel restores header");
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

    /// The done card surfaces download/open/delete overlay buttons. Buttons
    /// are icon-labelled now (cached `<img>`s served from /static/icons), so
    /// we assert on their `title` tooltips (which also keep the actions
    /// accessible) and that an `<img>` is rendered for each.
    #[test]
    fn done_card_overlay_has_actions() {
        let mut it = item(ItemStatus::Done, None);
        it.filename = Some("video.webm".into());
        let html = render_card(&it);
        assert!(html.contains(r#"title="download to this device"#));
        assert!(html.contains(r#"title="open/preview"#));
        assert!(html.contains(r#"title="delete from server"#));
        // Each action button embeds an <img> icon (download/open/delete
        // + the always-present logs button = 4 icons).
        assert_eq!(html.matches("<img").count(), 4, "download/open/delete/logs icons");
        assert!(html.contains(r#"src="/static/icons/download.svg"#), "download icon url");
        assert!(html.contains(r#"src="/static/icons/play.svg"#), "open icon url");
        assert!(html.contains(r#"src="/static/icons/trash.svg"#), "delete icon url");
        assert!(html.contains(r#"src="/static/icons/logs.svg"#), "logs icon url");
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
        assert!(html.contains(r#"title="cancel download"#));
        assert_eq!(html.matches("<img").count(), 2, "cancel + logs icons");
        assert!(html.contains(r#"src="/static/icons/stop.svg"#), "cancel icon url");
        assert!(html.contains(r#"src="/static/icons/logs.svg"#), "logs icon url");
        // No done-state actions on a pending card.
        assert!(!html.contains(r#"title="download to this device"#));
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