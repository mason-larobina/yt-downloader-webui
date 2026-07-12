//! Download-directory library: scan, stream files, delete files.
use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use tokio::io::AsyncSeekExt;
use tokio::io::AsyncReadExt;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::render::{esc, render_library_scan};
use crate::state::AppState;

/// A regular file discovered in the download directory.
#[derive(Debug, Clone)]
pub struct LibraryFile {
    pub name: String,
    pub size: u64,
    pub mtime: time::OffsetDateTime,
}

/// Non-recursive scan of `dir` for regular files, newest first.
pub fn scan(dir: &Path) -> std::io::Result<Vec<LibraryFile>> {
    let mut files: Vec<LibraryFile> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Hidden files starting with '.' are skipped (yt-dlp may write .part files).
        if name.starts_with('.') {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| {
                let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                time::OffsetDateTime::from_unix_timestamp(dur.as_secs() as i64).ok()
            })
            .unwrap_or_else(time::OffsetDateTime::now_utc);
        files.push(LibraryFile {
            name,
            size: meta.len(),
            mtime,
        });
    }
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    Ok(files)
}

/// Resolve `name` to a path inside `dir`, guarding against traversal.
/// Returns `None` if the name is unsafe or escapes `dir`.
pub fn resolve_safe(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == ".."
        || name.contains('\0')
        || name.starts_with('.')
    {
        return None;
    }
    let target = dir.join(name);
    // Canonicalize the parent and rejoin, then verify the resolved path stays
    // under the dir. We avoid requiring the file to exist via canonicalize on
    // the target (it does for /file and /delete which operate on existing
    // files); if it doesn't exist, return None.
    let dir_canon = dir.canonicalize().ok()?;
    let target_canon = target.canonicalize().ok()?;
    if target_canon.starts_with(&dir_canon) {
        Some(target_canon)
    } else {
        None
    }
}

/// GET /library -- render the file list as an HTML fragment.
pub async fn get_library(axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>) -> String {
    render_library_scan(&state.cfg.download_dir)
}

/// Query params for /file/:name.
#[derive(Deserialize, Default)]
pub struct FileQuery {
    pub inline: Option<String>,
    pub download: Option<String>,
}

/// GET /file/:name -- stream a file (inline or attachment), with single-range
/// support for media seeking on mobile.
pub async fn get_file(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<FileQuery>,
    range_hdr: axum::http::HeaderMap,
) -> Response {
    serve_file(&state.cfg.download_dir, &name, q, &range_hdr).await
}

async fn serve_file(dir: &Path, name: &str, q: FileQuery, headers: &HeaderMap) -> Response {
    let path = match resolve_safe(dir, name) {
        Some(p) => p,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let meta = match file.metadata().await {
        Ok(m) => m,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let total = meta.len();

    let disposition = if q.inline.is_some() {
        "inline"
    } else {
        "attachment"
    };

    let content_type = mime_guess::from_path(&path)
        .first()
        .map(|m| m.essence_str().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let filename_hdr = format!(
        "{}; filename=\"{}\"",
        disposition,
        esc(name).replace('"', "")
    );

    // Parse a single-range request like "bytes=START-END" or "bytes=START-".
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);

    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type).unwrap(),
    );
    out.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&filename_hdr).unwrap(),
    );
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    match range {
        Some((start, end)) if start < total => {
            let end = end.min(total - 1);
            let len = end - start + 1;
            let mut reader = file;
            if reader.seek(SeekFrom::Start(start)).await.is_err() {
                return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            }
            let stream = tokio_util::io::ReaderStream::with_capacity(
                reader.take(len),
                64 * 1024,
            );
            let body = Body::from_stream(stream);
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
            out.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
            );
            (StatusCode::PARTIAL_CONTENT, out, body).into_response()
        }
        _ => {
            let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
            let body = Body::from_stream(stream);
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&total.to_string()).unwrap(),
            );
            (StatusCode::OK, out, body).into_response()
        }
    }
}

/// Parse a single `bytes=START-END` / `bytes=START-` range. Returns (start, end_inclusive).
fn parse_range(h: &str) -> Option<(u64, u64)> {
    let s = h.strip_prefix("bytes=")?;
    let (start_s, end_s) = s.split_once('-')?;
    let start: u64 = start_s.parse().ok()?;
    let end: u64 = if end_s.is_empty() {
        u64::MAX
    } else {
        end_s.parse().ok()?
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

/// POST /delete/:name -- delete a file and refresh every tab's library.
pub async fn delete_file(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> String {
    match resolve_safe(&state.cfg.download_dir, &name) {
        Some(path) => match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                let frag = render_library_scan(&state.cfg.download_dir);
                state.emit(crate::events::Event::Library(frag));
                format!(r#"<span id="ack">deleted {}</span>"#, esc(&name))
            }
            Err(e) => format!(
                r#"<span id="ack" class="err">failed to delete {}: {}</span>"#,
                esc(&name),
                esc(&e.to_string())
            ),
        },
        None => format!(r#"<span id="ack" class="err">no such file</span>"#),
    }
}
