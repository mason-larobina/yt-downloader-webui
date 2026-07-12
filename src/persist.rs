//! Per-item JSON persistence: one file per queue item, named
//! `<unix_ts>.json` where the timestamp is the item's enqueue time. When two
//! items share the same second the filename timestamp is **numerically
//! incremented** (`<ts>.json`, `<ts+1>.json`, ...) until a free slot is found.
//!
//! On startup every `*.json` in the state dir is loaded; the queue is ordered
//! FIFO by enqueue timestamp (then by id for stability). Restart requeue
//! (`active` -> `pending`) is applied on load.
//!
//! `save` reconciles the directory to **mirror the live queue**: each item is
//! (re)written to its own file, and files whose id is no longer in the queue
//! (cleared / history-trimmed items) are deleted. Writes are atomic per file
//! (`.json.tmp` + fsync + rename).
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339 as Rfc3339Fmt;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::state::{ItemStatus, Queue, QueueItem};

const STATE_VERSION: u64 = 1;

/// The serialized projection of a single queue item. One per file.
#[derive(Serialize, Deserialize)]
struct SerializedItem {
    version: u64,
    id: u64,
    url: String,
    status: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    filename: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    error: Option<String>,
    /// Per-item yt-dlp log lines (capped). Defaults to empty for older state
    /// files written before the logs pane existed.
    #[serde(default)]
    logs: Vec<String>,
    enqueued_at: String,
}

/// Load every `<ts>.json` file in `dir`, reconstruct the in-memory queue, and
/// apply restart semantics (`active` -> `pending`). Items are ordered FIFO by
/// `enqueued_at` (then `id`). A per-file parse error moves that single file
/// aside to `<name>.bad-<ts>`; the rest of the queue still loads.
pub async fn load(dir: &Path, cache_dir: &Path) -> Result<Queue> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating state dir {}", dir.display()))?;

    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading state dir {}", dir.display()));
        }
    };

    let mut items: Vec<QueueItem> = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Only consume `<digits>.json`; skip temp / moved-aside files.
        if !is_state_file(name) {
            continue;
        }
        match load_one(&path, cache_dir).await {
            Ok(item) => items.push(item),
            Err(e) => {
                let bad = move_aside(&path).await?;
                tracing::warn!(
                    "state file {} failed to load ({}); moved aside to {} -- skipping this item",
                    path.display(),
                    e,
                    bad.display()
                );
            }
        }
    }

    // FIFO by enqueue time; id breaks ties (and survives same-second enqueues).
    items.sort_by(|a, b| {
        a.enqueued_at
            .cmp(&b.enqueued_at)
            .then(a.id.cmp(&b.id))
    });

    let max_id = items.iter().map(|i| i.id).max().unwrap_or(0);
    let mut queue = Queue::new();
    queue.next_id = max_id.max(1) + 1;
    queue.items = items;

    tracing::info!(
        "loaded queue: {} items, {} pending",
        queue.items.len(),
        queue
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Pending)
            .count()
    );
    Ok(queue)
}

/// Parse one `<ts>.json` into a `QueueItem` (applying `active` -> `pending`).
/// A persisted `thumbnail` whose cache file no longer exists (cache cleared) is
/// dropped to `None` so the UI never renders a broken image -- the cache is
/// re-generatable on the next probe / download.
async fn load_one(path: &Path, cache_dir: &Path) -> Result<QueueItem> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let s: SerializedItem =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;

    if s.version > STATE_VERSION {
        bail!(
            "unknown version {} (expected <= {})",
            s.version,
            STATE_VERSION
        );
    }

    let status = match s.status.as_str() {
        "pending" => ItemStatus::Pending,
        "active" => ItemStatus::Pending, // re-queue on restart
        "done" => ItemStatus::Done,
        "failed" => ItemStatus::Failed,
        "cancelled" => ItemStatus::Cancelled,
        other => bail!("unknown item status {other:?}"),
    };

    let enqueued_at =
        OffsetDateTime::parse(&s.enqueued_at, &Rfc3339Fmt)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());

    // Self-heal: if the persisted thumbnail file is gone from the cache dir,
    // treat it as absent (the cache is safe to clear; the thumb is re-fetched
    // / re-generated on the next probe / download).
    let thumbnail = s.thumbnail.filter(|name| cache_dir.join(name).is_file());

    Ok(QueueItem {
        id: s.id,
        url: s.url,
        status,
        title: s.title,
        duration: s.duration,
        filename: s.filename,
        thumbnail,
        progress: None,
        error: s.error,
        logs: s.logs,
        cancel: None,
        enqueued_at,
    })
}

/// Reconcile `dir` to mirror the live queue. Each item is (re)written to its
/// own `<ts>.json` (reusing the item's existing file when one exists;
/// otherwise allocating `<enqueued_at_ts>.json`, bumping numerically on
/// collision). Files whose id is no longer in `items` (cleared / trimmed) are
/// deleted. Takes a snapshot so callers need not hold the queue lock across
/// the write.
pub async fn save(dir: &Path, _next_id: u64, items: &[QueueItem]) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating state dir {}", dir.display()))?;

    // Map existing file id -> path, and the set of taken filename timestamps.
    let existing = scan_existing(dir).await;
    let mut taken_ts: HashSet<i64> = existing
        .values()
        .filter_map(|p| filename_ts(p))
        .collect();

    let live_ids: HashSet<u64> = items.iter().map(|i| i.id).collect();

    // (Re)write each live item to its file.
    for item in items {
        let path = match existing.get(&item.id) {
            Some(p) => p.clone(),
            None => {
                // Allocate a free <ts>.json starting at the enqueue second.
                let mut ts = item.enqueued_at.unix_timestamp();
                while taken_ts.contains(&ts) {
                    ts += 1;
                }
                taken_ts.insert(ts);
                dir.join(format!("{ts}.json"))
            }
        };
        write_item(&path, item).await?;
    }

    // Delete orphaned files (ids no longer in the queue -- cleared / trimmed).
    for (id, path) in &existing {
        if !live_ids.contains(id) && let Err(e) = tokio::fs::remove_file(path).await {
            tracing::debug!("could not remove orphaned state file {}: {e}", path.display());
        }
    }

    Ok(())
}

/// Scan `dir` for `<digits>.json` files and map `id -> path` by parsing each.
async fn scan_existing(dir: &Path) -> HashMap<u64, PathBuf> {
    let mut map = HashMap::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return map,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !is_state_file(name) {
            continue;
        }
        if let Ok(bytes) = tokio::fs::read(&path).await
            && let Ok(s) = serde_json::from_slice::<SerializedItem>(&bytes)
        {
            map.insert(s.id, path);
        }
        // Unparseable files are left alone; `load` moves them aside.
    }
    map
}

/// Atomically write one item to `path` (serialize to `.json.tmp`, fsync, rename).
async fn write_item(path: &Path, item: &QueueItem) -> Result<()> {
    let s = SerializedItem {
        version: STATE_VERSION,
        id: item.id,
        url: item.url.clone(),
        status: item.status.as_str().to_string(),
        title: item.title.clone(),
        duration: item.duration,
        filename: item.filename.clone(),
        thumbnail: item.thumbnail.clone(),
        error: item.error.clone(),
        logs: item.logs.clone(),
        enqueued_at: item
            .enqueued_at
            .format(&Rfc3339Fmt)
            .unwrap_or_default(),
    };
    let json = serde_json::to_vec_pretty(&s).context("serializing item")?;

    let tmp = sibling_tmp(path);
    {
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating temp state file {}", tmp.display()))?;
        f.write_all(&json).await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming temp state file into {}", path.display()))?;
    Ok(())
}

/// `<dir>/<name>.json` -> `<dir>/<name>.json.tmp` (kept sibling for atomic rename).
fn sibling_tmp(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("item.json"));
    name.push(".tmp");
    path.with_file_name(name)
}

/// True for filenames of the form `<digits>.json` (excludes `.tmp` / `.bad-*`).
fn is_state_file(name: &str) -> bool {
    let stem = match name.strip_suffix(".json") {
        Some(s) => s,
        None => return false,
    };
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit())
}

/// Parse the leading numeric timestamp from a `<digits>.json` filename.
fn filename_ts(path: &Path) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".json")?;
    stem.parse::<i64>().ok()
}

/// Move `path` aside to `<name>.bad-<unix_ts>`. Returns the new path.
async fn move_aside(path: &Path) -> Result<PathBuf> {
    let ts = OffsetDateTime::now_utc().unix_timestamp();
    let mut bad = path.to_path_buf();
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("item.json"));
    name.push(format!(".bad-{ts}"));
    bad.set_file_name(name);
    let _ = tokio::fs::rename(path, &bad).await;
    Ok(bad)
}
