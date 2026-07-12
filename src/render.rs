//! Render server-side HTML fragments for SSE events.
use std::path::Path;

use crate::state::{ItemStatus, Queue, QueueItem};
use crate::library::LibraryFile;

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

// ----------------------------- #status ------------------------------------

/// Render the `#status` fragment: the active item's progress bar, or an idle
/// "queue empty / waiting" line.
pub fn render_status(active: Option<&QueueItem>) -> String {
    match active {
        None => r#"<div id="status" class="status idle">queue empty &mdash; waiting for URLs</div>"#
            .to_string(),
        Some(item) => {
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

            let mut bits: Vec<String> = Vec::new();
            if !speed.is_empty() { bits.push(speed.clone()); }
            if !eta.is_empty() { bits.push(eta.clone()); }
            if let Some(b) = bytes {
                if !b.is_empty() { bits.push(b); }
            }

            format!(
                r#"<div id="status" class="status active"><div class="bar"><i style="width:{w:.0}%"></i></div><span class="pct">{pct:.0}%</span> <span class="label">{label}</span>{meta}</div>"#,
                w = width,
                pct = percent,
                label = label,
                meta = if bits.is_empty() {
                    String::new()
                } else {
                    format!(" <span class=\"meta\">{}</span>", bits.join(" | "))
                }
            )
        }
    }
}

// ----------------------------- #queue --------------------------------------

/// Render the full `#queue` fragment.
pub fn render_queue(queue: &Queue) -> String {
    let mut rows = String::new();
    for item in &queue.items {
        rows.push_str(&render_queue_row(item));
    }

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

    let clear_btn = if terminal > 0 {
        format!(
            r##"<button class="clear" hx-post="/clear" hx-target="#ack" hx-swap="innerHTML">clear {n}</button>"##,
            n = terminal
        )
    } else {
        String::new()
    };

    if queue.items.is_empty() {
        return r#"<div id="queue" class="queue"><div class="empty">no items</div></div>"#.to_string();
    }

    format!(
        r#"<div id="queue" class="queue"><div class="qhead">queue ({items}, {pending} pending)</div>{rows}{clear}</div>"#,
        items = queue.items.len(),
        pending = pending,
        rows = rows,
        clear = clear_btn,
    )
}

fn status_glyph(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Pending => ".",
        ItemStatus::Active => "~",
        ItemStatus::Done => "v",
        ItemStatus::Failed => "x",
        ItemStatus::Cancelled => "-",
    }
}

fn render_queue_row(item: &QueueItem) -> String {
    let glyph = status_glyph(item.status);
    let label = esc(item.label());
    let status = item.status.as_str();

    let action = match item.status {
        ItemStatus::Pending => format!(
            r##"<button class="cancel" hx-post="/cancel/{id}" hx-target="#ack" hx-swap="innerHTML">cancel</button>"##,
            id = item.id
        ),
        ItemStatus::Active => format!(
            r##"<button class="cancel" hx-post="/cancel/{id}" hx-target="#ack" hx-swap="innerHTML">cancel</button>"##,
            id = item.id
        ),
        ItemStatus::Failed | ItemStatus::Cancelled => format!(
            r##"<button class="retry" hx-post="/retry/{id}" hx-target="#ack" hx-swap="innerHTML">retry</button>"##,
            id = item.id
        ),
        ItemStatus::Done => match &item.filename {
            Some(name) => format!(
                r##"<a class="dl" href="/file/{name}?download=1">download</a>"##,
                name = esc(name)
            ),
            None => String::new(),
        },
    };

    let error = match (&item.status, &item.error) {
        (ItemStatus::Failed, Some(e)) => format!(r#" <span class="err">error: {e}</span>"#, e = esc(e)),
        _ => String::new(),
    };

    format!(
        r##"<div class="row {status}"><span class="glyph">{g}</span> <span class="label">{label}</span> <span class="st">({status})</span> {action}{error}</div>"##,
        g = glyph,
        label = label,
        status = status,
        action = action,
        error = error,
    )
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
    let size = human_bytes(f.size);
    let mtime = f
        .mtime
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_default();
    format!(
        r##"<div class="lrow"><span class="name">{name}</span> <span class="size">{size}</span> <span class="mtime">{mtime}</span> <a class="open" href="/file/{name}?inline=1">open</a> <a class="dl" href="/file/{name}?download=1">download</a> <button class="del" hx-post="/delete/{name}" hx-target="#ack" hx-swap="innerHTML" hx-confirm="Delete {name}?">delete</button></div>"##,
        name = name,
        size = size,
        mtime = mtime,
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
