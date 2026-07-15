//! Download-directory library: scan, stream files, delete files.
use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::render::{esc, render_ack};
use crate::state::AppState;

/// A regular file discovered in the download directory.
#[derive(Debug, Clone)]
pub struct LibraryFile {
    pub name: String,
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
        // Skip dotfiles (hidden) and yt-dlp's partial-download fragments, which
        // end in `.part` -- neither is a finished file to import/serve.
        if name.starts_with('.') || name.ends_with(".part") {
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
        files.push(LibraryFile { name, mtime });
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.mtime));
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
    // Canonicalize both `dir` and the target, then require the resolved
    // target to stay under `dir`. The earlier string checks block the obvious
    // traversal names; canonicalization resolves symlinks, so a `dir` entry
    // that is a symlink pointing outside is rejected by the `starts_with`
    // check. `canonicalize` requires the path to exist (returns `None` for a
    // missing file), which is fine here -- /file and /delete only ever
    // operate on existing files, and a miss maps to a 404, never an error
    // that leaks whether an out-of-dir path existed.
    let dir_canon = dir.canonicalize().ok()?;
    let target_canon = target.canonicalize().ok()?;
    if target_canon.starts_with(&dir_canon) {
        Some(target_canon)
    } else {
        None
    }
}

/// Query params for /file/:name. `?inline=1` serves the file inline (for
/// in-browser preview); the default is an attachment (download).
#[derive(Deserialize, Default)]
pub struct FileQuery {
    pub inline: Option<String>,
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
            let stream = tokio_util::io::ReaderStream::with_capacity(reader.take(len), 64 * 1024);
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

/// POST /delete/:name -- delete a file from the download directory.
pub async fn delete_file(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> String {
    match resolve_safe(&state.cfg.download_dir, &name) {
        Some(path) => match tokio::fs::remove_file(&path).await {
            Ok(()) => render_ack(&format!("deleted {}", name), false),
            Err(e) => render_ack(&format!("failed to delete {}: {}", name, e), true),
        },
        None => render_ack("no such file", true),
    }
}
