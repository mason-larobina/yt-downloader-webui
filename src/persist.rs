//! Atomic load/save of the queue to `queue.json`, with restart requeue.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339 as Rfc3339Fmt;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::state::{ItemStatus, Queue, QueueItem};

const STATE_VERSION: u64 = 1;

/// The serialized projection of the queue (per DESIGN Sec. 8).
#[derive(Serialize, Deserialize)]
struct StateFile {
    version: u64,
    next_id: u64,
    items: Vec<SerializedItem>,
}

#[derive(Serialize, Deserialize)]
struct SerializedItem {
    id: u64,
    url: String,
    status: String,
    filename: Option<String>,
    error: Option<String>,
    enqueued_at: String,
}

/// Load `queue.json`, reconstruct the in-memory queue, and apply restart
/// semantics: `active` -> `pending`; `pending` -> `pending`; terminal items
/// kept as history. On parse error the file is moved aside to
/// `queue.json.bad-<ts>` and an empty queue starts.
pub async fn load(path: &Path) -> Result<Queue> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("state file absent; starting with empty queue");
            return Ok(Queue::new());
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading state file {}", path.display()));
        }
    };

    let state: StateFile = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            let bad = move_aside(path).await?;
            tracing::warn!(
                "state file {} failed to parse ({}); moved aside to {} -- starting empty",
                path.display(),
                e,
                bad.display()
            );
            return Ok(Queue::new());
        }
    };

    if state.version > STATE_VERSION {
        let bad = move_aside(path).await?;
        tracing::warn!(
            "state file {} has unknown version {} (expected <= {}); moved aside to {} -- starting empty",
            path.display(),
            state.version,
            STATE_VERSION,
            bad.display()
        );
        return Ok(Queue::new());
    }

    let mut queue = Queue::new();
    queue.next_id = state.next_id.max(1);

    for s in state.items {
        let status = match s.status.as_str() {
            "pending" => ItemStatus::Pending,
            "active" => ItemStatus::Pending, // re-queue
            "done" => ItemStatus::Done,
            "failed" => ItemStatus::Failed,
            "cancelled" => ItemStatus::Cancelled,
            other => {
                tracing::warn!("unknown item status {other:?} for item {}; skipping", s.id);
                continue;
            }
        };
        let enqueued_at = OffsetDateTime::parse(
            &s.enqueued_at,
            &Rfc3339Fmt,
        )
        .unwrap_or_else(|_| OffsetDateTime::now_utc());

        queue.items.push(QueueItem {
            id: s.id,
            url: s.url,
            status,
            filename: s.filename,
            progress: None, // runtime-only
            error: s.error,
            cancel: None,
            enqueued_at,
        });
        if s.id >= queue.next_id {
            queue.next_id = s.id + 1;
        }
    }

    tracing::info!(
        "loaded queue: {} items, {} pending",
        queue.items.len(),
        queue.items.iter().filter(|i| i.status == ItemStatus::Pending).count()
    );
    Ok(queue)
}

/// Atomically save the queue to `path` (serialize to `.tmp`, fsync, rename).
/// Takes a snapshot so callers need not hold the queue lock across the write.
pub async fn save(path: &Path, next_id: u64, items: &[QueueItem]) -> Result<()> {
    let items: Vec<SerializedItem> = items
        .iter()
        .map(|i| SerializedItem {
            id: i.id,
            url: i.url.clone(),
            status: i.status.as_str().to_string(),
            filename: i.filename.clone(),
            error: i.error.clone(),
            enqueued_at: i.enqueued_at.format(&Rfc3339Fmt).unwrap_or_default(),
        })
        .collect();

    let state = StateFile {
        version: STATE_VERSION,
        next_id,
        items,
    };

    let json = serde_json::to_vec_pretty(&state).context("serializing queue")?;

    let tmp = path.with_extension("json.tmp");
    // Write temp file in the same directory as the target so the rename is atomic.
    let tmp = ensure_sibling(path, &tmp);

    {
        let mut f = tokio::fs::File::create(&tmp).await.with_context(|| {
            format!("creating temp state file {}", tmp.display())
        })?;
        f.write_all(&json).await?;
        f.sync_all().await?;
    }

    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming temp state file into {}", path.display()))?;

    Ok(())
}

/// Ensure `tmp` is in the same directory as `dst` (so rename is atomic). Falls
/// back to a sibling filename next to `dst`.
fn ensure_sibling(dst: &Path, tmp: &Path) -> PathBuf {
    if tmp.parent() == dst.parent() {
        return tmp.to_path_buf();
    }
    // Fall back: <dst>.tmp in the same dir.
    let name = dst
        .file_name()
        .map(|n| {
            let mut n = n.to_os_string();
            n.push(".tmp");
            n
        })
        .unwrap_or_else(|| std::ffi::OsString::from("queue.json.tmp"));
    dst.with_file_name(name)
}

/// Move `path` aside to `path.bad-<unix_ts>`. Returns the new path.
async fn move_aside(path: &Path) -> Result<PathBuf> {
    let ts = OffsetDateTime::now_utc().unix_timestamp();
    let mut bad = path.to_path_buf();
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("queue.json"));
    name.push(format!(".bad-{ts}"));
    bad.set_file_name(name);
    let _ = tokio::fs::rename(path, &bad).await;
    Ok(bad)
}
