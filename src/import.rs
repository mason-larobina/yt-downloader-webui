//! Import / reconcile: keep the download directory, the per-item state
//! index, and the thumbnail cache in sync. Runs on startup and after every
//! successful download.
//!
//! 1. **Dedupe** -- at most one Done state file per on-disk video filename.
//!    When several Done items reference the same file (e.g. an imported item
//!    plus a later real download of the same file, or stale duplicates), the
//!    "best" one is kept and the rest are dropped (their state files are
//!    deleted by the next `persist::save` reconciliation).
//! 2. **Prune missing** -- a Done item whose on-disk video has been moved or
//!    deleted is removed from the queue (its state file is deleted by
//!    `persist::save`). A moved file is then re-imported in step 3 under its
//!    new name. Pending / Active / Failed / Cancelled items are never pruned
//!    here -- they represent attempts, not videos on disk.
//! 3. **Import** -- unreferenced video files (no Done item names them) are
//!    ffprobed; those with a video stream become Done items with an empty
//!    `url` (distinguishing them from real downloads), `media` populated,
//!    and `enqueued_at` set to the file's mtime so they sort correctly.
//! 4. **Probe media** -- Done items missing `media` (older state files
//!    predating this feature, or a freshly-finished download) are probed and
//!    their `duration` is filled from the probe if previously unknown.
//! 5. **Garbage-collect thumbs** -- cache files not referenced by any live
//!    item's `thumbnail`/`thumbnails` are deleted: thumbs owned by pruned /
//!    deduped items, by externally-deleted videos, or any stray cache file.
//!    The cache dir is a pure cache, so unreferenced files are safe to drop.
//! 6. **Generate thumbnails** -- Done items missing `thumbnails` get native
//!    `1.5*ln(duration)` frames extracted (background, best-effort).
//!
//! ffprobe results are persisted on the item so we never re-probe the same
//! file; thumbnail generation is idempotent (cache reuse) so a repeat run
//! only fills gaps. Thumb ownership: each state file owns its thumbs via the
//! `thumbnail` (primary) + `thumbnails` (native gallery) fields; step 5
//! enforces that no cache file outlives the state file that owns it.
use std::collections::HashMap;
use std::sync::Arc;

use crate::events::Event;
use crate::media::{self, MediaInfo};
use crate::render;
use crate::state::{AppState, ItemStatus, QueueItem};
use crate::thumb;

/// Per-ffprobe timeout. ffprobe is container-only (no decode) and fast, but a
/// corrupted/odd file can hang it; bound it so one bad file can't stall import.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the full reconcile: dedupe, prune missing files, import unreferenced
/// videos, probe missing media, garbage-collect orphan thumbnails, then spawn
/// background thumbnail generation for any Done item still missing its native
/// frames. Safe to call repeatedly and concurrently -- every step is
/// idempotent and lock-guarded.
pub async fn reconcile(state: Arc<AppState>) {
    if state.shutdown.is_cancelled() {
        return;
    }

    let mut changed = false;
    changed |= dedupe_done_per_filename(&state).await;
    changed |= prune_missing_files(&state).await;
    changed |= import_unreferenced(&state).await;
    changed |= probe_missing_media(&state).await;

    if changed {
        state.persist().await;
        let q = state.queue.lock().await;
        state.emit(Event::Queue(render::render_queue(&q)));
    }

    // Garbage-collect orphaned thumbnails (owned by pruned / deduped items or
    // by externally-deleted videos). Runs after persist so the state dir
    // already mirrors the pruned queue; safe to run every time.
    garbage_collect_thumbs(&state).await;

    // Always run thumbnail generation -- it self-skips items that already have
    // frames, so it is cheap when there is nothing to do.
    spawn_thumbnail_generation(state.clone());
}

/// Step 1: for each on-disk filename referenced by more than one Done item,
/// keep the best (prefer a real download with a url > has media > has
/// thumbnails > most recently enqueued) and drop the rest. Returns true if any
/// item was removed.
async fn dedupe_done_per_filename(state: &Arc<AppState>) -> bool {
    let mut by_file: HashMap<String, Vec<u64>> = HashMap::new();
    {
        let q = state.queue.lock().await;
        for item in &q.items {
            if item.status == ItemStatus::Done
                && let Some(name) = &item.filename
            {
                by_file.entry(name.clone()).or_default().push(item.id);
            }
        }
    }

    let mut to_remove: Vec<u64> = Vec::new();
    for ids in by_file.values() {
        if ids.len() < 2 {
            continue;
        }
        // Pick the best id; the rest are removed.
        let q = state.queue.lock().await;
        let best = ids
            .iter()
            .copied()
            .max_by_key(|id| q.get(*id).map(score_item));
        drop(q);
        for id in ids {
            if Some(*id) != best {
                to_remove.push(*id);
            }
        }
    }

    if to_remove.is_empty() {
        return false;
    }
    let n = {
        let mut q = state.queue.lock().await;
        let mut n = 0;
        for id in &to_remove {
            if q.remove_terminal(*id) {
                n += 1;
            }
        }
        n
    };
    if n > 0 {
        tracing::info!("dedupe: removed {n} duplicate Done item(s)");
    }
    n > 0
}

/// Higher is better. Prefer a real download (non-empty url) over an import,
/// then one already carrying media + thumbnails, then the most recently
/// enqueued.
fn score_item(item: &QueueItem) -> (u8, u8, u8, i64) {
    (
        u8::from(!item.url.is_empty()),
        u8::from(item.media.is_some()),
        u8::from(!item.thumbnails.is_empty()),
        item.enqueued_at.unix_timestamp(),
    )
}

/// Step 2: prune Done items whose on-disk video has been moved or deleted.
/// A Done item represents a video file on disk; if that file is gone the
/// state file is stale and is removed (its state file is deleted by the next
/// `persist::save`, and its owned thumbnails are garbage-collected in step 5).
/// Pending / Active / Failed / Cancelled items are never pruned here -- they
/// represent download attempts, not videos, and a failed/cancelled item may
/// legitimately be retried. Returns true if any item was pruned.
async fn prune_missing_files(state: &Arc<AppState>) -> bool {
    // Set of filenames currently present in the download dir.
    let present: std::collections::HashSet<String> = match crate::library::scan(&state.cfg.download_dir) {
        Ok(files) => files.into_iter().map(|f| f.name).collect(),
        Err(e) => {
            tracing::warn!("prune: scan failed: {e}");
            return false;
        }
    };

    // Done items whose filename is missing on disk.
    let to_remove: Vec<u64> = {
        let q = state.queue.lock().await;
        q.items
            .iter()
            .filter(|i| i.status == ItemStatus::Done)
            .filter(|i| match &i.filename {
                Some(name) => !present.contains(name),
                None => false, // no filename -> nothing to prune (no file to own)
            })
            .map(|i| i.id)
            .collect()
    };

    if to_remove.is_empty() {
        return false;
    }
    let n = {
        let mut q = state.queue.lock().await;
        let mut n = 0;
        for id in &to_remove {
            if q.remove_terminal(*id) {
                n += 1;
            }
        }
        n
    };
    if n > 0 {
        tracing::info!("prune: removed {n} Done item(s) whose file is missing");
    }
    n > 0
}

/// Step 3: scan the download dir; for each file not referenced by any Done
/// item, ffprobe it and -- if it has a video stream -- create a Done item.
/// Returns true if any item was imported.
async fn import_unreferenced(state: &Arc<AppState>) -> bool {
    let files = match crate::library::scan(&state.cfg.download_dir) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("import: scan failed: {e}");
            return false;
        }
    };

    // Filenames already claimed by a Done item.
    let referenced: std::collections::HashSet<String> = {
        let q = state.queue.lock().await;
        q.items
            .iter()
            .filter(|i| i.status == ItemStatus::Done)
            .filter_map(|i| i.filename.clone())
            .collect()
    };

    let ffprobe = &state.cfg.ffprobe;
    let download_dir = &state.cfg.download_dir;
    let mut imported: Vec<(QueueItem, MediaInfo)> = Vec::new();

    for f in &files {
        if referenced.contains(&f.name) {
            continue;
        }
        if state.shutdown.is_cancelled() {
            break;
        }
        let path = match crate::library::resolve_safe(download_dir, &f.name) {
            Some(p) => p,
            None => continue,
        };
        let media = match tokio::time::timeout(
            PROBE_TIMEOUT,
            media::probe(ffprobe, &path),
        )
        .await
        {
            Ok(Ok(m)) if m.has_video() => m,
            Ok(Ok(_)) => continue, // not a video (audio/data) -- skip
            Ok(Err(e)) => {
                tracing::debug!("import: ffprobe failed for {}: {e:#}", f.name);
                continue;
            }
            Err(_) => {
                tracing::warn!("import: ffprobe timed out for {}", f.name);
                continue;
            }
        };
        let mut item = QueueItem::new(0, String::new());
        item.status = ItemStatus::Done;
        item.filename = Some(f.name.clone());
        item.duration = media.duration;
        item.enqueued_at = f.mtime;
        imported.push((item, media));
    }

    if imported.is_empty() {
        return false;
    }

    let n = imported.len();
    {
        let mut q = state.queue.lock().await;
        for (mut item, media) in imported {
            item.id = q.alloc_id();
            item.media = Some(media);
            q.items.push(item);
        }
    }
    tracing::info!("import: created {n} Done item(s) from unreferenced videos");
    true
}

/// Step 4: ffprobe Done items that have a filename but no `media` yet, and
/// fill `duration` if it was previously unknown. Returns true if any item was
/// updated.
async fn probe_missing_media(state: &Arc<AppState>) -> bool {
    let ffprobe = &state.cfg.ffprobe;
    let download_dir = &state.cfg.download_dir;

    // Snapshot of (id, filename) for Done items missing media.
    let needs: Vec<(u64, String)> = {
        let q = state.queue.lock().await;
        q.items
            .iter()
            .filter(|i| i.status == ItemStatus::Done && i.media.is_none())
            .filter_map(|i| i.filename.clone().map(|f| (i.id, f)))
            .collect()
    };

    if needs.is_empty() {
        return false;
    }

    let mut updated: Vec<(u64, MediaInfo)> = Vec::new();
    for (id, name) in &needs {
        if state.shutdown.is_cancelled() {
            break;
        }
        let path = match crate::library::resolve_safe(download_dir, name) {
            Some(p) => p,
            None => continue,
        };
        match tokio::time::timeout(PROBE_TIMEOUT, media::probe(ffprobe, &path)).await {
            Ok(Ok(m)) if m.has_video() => updated.push((*id, m)),
            Ok(Ok(_)) => continue, // file no longer a video / deleted
            Ok(Err(e)) => {
                tracing::debug!("import: ffprobe failed for item {id} ({name}): {e:#}");
                continue;
            }
            Err(_) => {
                tracing::warn!("import: ffprobe timed out for item {id} ({name})");
                continue;
            }
        }
    }

    if updated.is_empty() {
        return false;
    }
    {
        let mut q = state.queue.lock().await;
        for (id, media) in &updated {
            if let Some(item) = q.get_mut(*id) {
                if item.duration.is_none() {
                    item.duration = media.duration;
                }
                item.media = Some(media.clone());
            }
        }
    }
    tracing::info!("import: probed media for {} item(s)", updated.len());
    true
}

/// Step 5: garbage-collect the thumbnail cache. Each state file owns its
/// thumbs via the `thumbnail` (primary) and `thumbnails` (native gallery)
/// fields; any cache file not referenced by any live item is an orphan and
/// is deleted. This reclaims thumbs owned by pruned / deduped items (whose
/// videos were moved or deleted), by externally-deleted videos, and any stray
/// cache file. In-flight temp files (`.tmp`, dotfiles) are left alone so a
/// concurrent fetch / ffmpeg pass isn't disturbed. Safe to run every time.
async fn garbage_collect_thumbs(state: &Arc<AppState>) {
    // The set of thumb filenames still referenced by some live item.
    let referenced: std::collections::HashSet<String> = {
        let q = state.queue.lock().await;
        let mut set = std::collections::HashSet::new();
        for item in &q.items {
            if let Some(name) = &item.thumbnail {
                set.insert(name.clone());
            }
            for name in &item.thumbnails {
                set.insert(name.clone());
            }
        }
        set
    };

    let mut rd = match tokio::fs::read_dir(&state.cfg.cache_dir).await {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!("gc: could not read cache dir {}: {e}", state.cfg.cache_dir.display());
            return;
        }
    };

    let mut removed = 0u64;
    while let Some(entry) = rd.next_entry().await.unwrap_or(None) {
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        // Skip in-flight temp / hidden files (never referenced by an item).
        if name.starts_with('.') || name.ends_with(".tmp") {
            continue;
        }
        if referenced.contains(&name) {
            continue;
        }
        // Orphan: not owned by any live state file. Best-effort delete.
        match tokio::fs::remove_file(entry.path()).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::debug!("gc: could not remove orphan thumb {name}: {e}"),
        }
    }
    if removed > 0 {
        tracing::info!("gc: removed {removed} orphaned thumbnail(s)");
    }
}

/// Step 6 (background): for each Done item with a present file but no native
/// frames, extract `1.5*ln(duration)` frames with ffmpeg into `thumbnails` (the
/// item-page gallery) and set the *primary* `thumbnail` as a fallback -- only
/// when no remote thumbnail was fetched (the remote thumb, if present, is the
/// preferred highest-quality primary). One spawned task processes items
/// sequentially (each item's frames are themselves sequential ffmpeg passes);
/// idempotent via cache reuse, so concurrent/overlapping runs only fill gaps.
fn spawn_thumbnail_generation(state: Arc<AppState>) {
    tokio::spawn(async move {
        let ffmpeg = state.cfg.ffmpeg.clone();
        let cache_dir = state.cfg.cache_dir.clone();
        let download_dir = state.cfg.download_dir.clone();

        // Snapshot of (id, filename, duration) for Done items missing thumbs.
        let needs: Vec<(u64, String, Option<f64>)> = {
            let q = state.queue.lock().await;
            q.items
                .iter()
                .filter(|i| i.status == ItemStatus::Done && i.thumbnails.is_empty())
                .filter_map(|i| {
                    i.filename
                        .clone()
                        .map(|f| (i.id, f, i.duration.or_else(|| i.media.as_ref().and_then(|m| m.duration))))
                })
                .collect()
        };

        if needs.is_empty() {
            return;
        }

        for (id, name, duration) in needs {
            if state.shutdown.is_cancelled() {
                break;
            }
            let path = match crate::library::resolve_safe(&download_dir, &name) {
                Some(p) => p,
                None => continue,
            };
            let frames = match thumb::generate_native(&ffmpeg, &cache_dir, &path, duration).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("import: thumbnail generation failed for item {id}: {e:#}");
                    continue;
                }
            };
            // Re-check under the lock: another concurrent run may have filled it.
            // The primary (remote) thumb, if already fetched, is left alone --
            // the native middle frame is only a fallback primary.
            let fallback_primary = frames.get(frames.len() / 2).cloned();
            let landed = {
                let mut q = state.queue.lock().await;
                let Some(item) = q.get_mut(id) else {
                    continue;
                };
                if !item.thumbnails.is_empty() {
                    continue; // already filled
                }
                item.thumbnails = frames.clone();
                if item.thumbnail.is_none() {
                    if let Some(p) = fallback_primary.clone() {
                        item.thumbnail = Some(p);
                    }
                }
                true
            };
            if landed {
                let q = state.queue.lock().await;
                state.emit(Event::Queue(render::render_queue(&q)));
                drop(q);
                state.persist().await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_prefers_url_then_media_then_thumbs_then_recent() {
        let now = time::OffsetDateTime::now_utc();
        let mut a = QueueItem::new(1, String::new()); // imported
        a.status = ItemStatus::Done;
        a.filename = Some("x.mp4".into());
        a.enqueued_at = now;

        let mut b = QueueItem::new(2, "https://u".into()); // real download
        b.status = ItemStatus::Done;
        b.filename = Some("x.mp4".into());
        b.enqueued_at = now;

        // b has a url -> scores higher even though both lack media/thumbs.
        assert!(score_item(&b) > score_item(&a));

        // Media boosts a real download above one without media.
        let mut c = QueueItem::new(3, "https://u".into());
        c.status = ItemStatus::Done;
        c.filename = Some("x.mp4".into());
        c.media = Some(MediaInfo::default());
        c.enqueued_at = now;
        assert!(score_item(&c) > score_item(&b));

        // More recent enqueued_at breaks ties among otherwise-equal items
        // (both have a url and media).
        let mut d = QueueItem::new(4, "https://u".into());
        d.status = ItemStatus::Done;
        d.filename = Some("x.mp4".into());
        d.media = Some(MediaInfo::default());
        d.enqueued_at = now + time::Duration::seconds(10);
        assert!(score_item(&d) > score_item(&c));
    }
}
