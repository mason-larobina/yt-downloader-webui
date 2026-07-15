//! Import / reconcile: keep the download directory, the per-item state
//! index, and the thumbnail cache in sync. Runs on startup and after every
//! successful download.
//!
//! 1. **Dedupe** -- at most one Done state file per on-disk video filename.
//!    When several Done items reference the same file (e.g. a re-download of
//!    a video already on disk, or stale duplicate state files), the **newest**
//!    item (most recently enqueued) is kept -- it retains its own id and queue
//!    position so the card the user was watching stays put -- and the older
//!    items' state (url / title / duration / media / thumbnails) is subsumed
//!    (merged) into it before they are dropped (their state files are deleted
//!    by the next `persist::save` reconciliation). A `toast` SSE event informs
//!    the user that the de-duplication happened, so it isn't silently buried.
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
//! 5. **Reset stale thumbnail counts** -- a Done item whose `thumbnails.len()`
//!    no longer matches `frame_count(duration)` (the formula or the probed
//!    duration changed since the frames were generated) has its native frames
//!    dropped (and its `thumbnail` cleared if it was a native fallback, not a
//!    fetched remote thumb). Step 6 then reclaims the orphaned files and step
//!    7 regenerates the correct count from scratch.
//! 6. **Garbage-collect thumbs** -- cache files not referenced by any live
//!    item's `thumbnail`/`thumbnails` are deleted: thumbs owned by pruned /
//!    deduped items, by externally-deleted videos, or any stray cache file.
//!    The cache dir is a pure cache, so unreferenced files are safe to drop.
//! 7. **Generate thumbnails** -- Done items missing `thumbnails` get native
//!    `frame_count(duration)` frames extracted (background, best-effort).
//!
//! ffprobe results are persisted on the item so we never re-probe the same
//! file; thumbnail generation is content-addressed (a re-run only re-extracts
//! frames whose content isn't already cached) so concurrent/overlapping runs
//! only fill gaps. Thumb ownership: each state file owns its thumbs via the
//! `thumbnail` (primary) + `thumbnails` (native gallery) fields; step 6
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
    changed |= reset_stale_thumbnail_counts(&state).await;

    if changed {
        state.persist().await;
        // Bulk reconciliation can touch many items at once (dedupe, prune,
        // import unreferenced, probe media), so a single full-grid `queue`
        // snapshot is the simplest correct reconcile. This runs only on
        // startup and once per completed download -- not the per-tick hot
        // path -- so it is not a flash concern. htmx reprocesses the swapped
        // nodes, re-binding every per-card `sse-swap` listener.
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
/// keep the **newest** (most recently enqueued) item and subsume the rest
/// into it before dropping them. The survivor keeps its own id and queue
/// position (so the card the user was watching stays put -- the common case
/// is a re-download of a file already on disk, where the just-finished item
/// is newest); each older item's state (url if the survivor has none, plus
/// title / duration / media / thumbnail / thumbnails the survivor is
/// missing) is merged into the survivor so nothing probed or thumbnailed on
/// the older item is lost. A `toast` SSE event announces the de-duplication
/// so it isn't silently buried in the download stack. Returns true if any
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

    // For each filename with duplicates, pick the newest id (the survivor);
    // subsume each older item's state into the survivor, then remove the
    // older items. Snapshot everything we need under one lock, mutate under
    // the next.
    struct Dedupe {
        survivor_id: u64,
        older_ids: Vec<u64>,
        filename: String,
    }
    let mut plans: Vec<Dedupe> = Vec::new();
    for ids in by_file.values() {
        if ids.len() < 2 {
            continue;
        }
        let q = state.queue.lock().await;
        // Newest by enqueued_at, breaking ties by id (higher == newer).
        let survivor_id = ids
            .iter()
            .copied()
            .max_by_key(|id| q.get(*id).map(|i| (i.enqueued_at.unix_timestamp(), i.id)))
            .expect("non-empty group");
        let filename = q
            .get(survivor_id)
            .and_then(|i| i.filename.clone())
            .unwrap_or_default();
        let older_ids: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| *id != survivor_id)
            .collect();
        drop(q);
        plans.push(Dedupe {
            survivor_id,
            older_ids,
            filename,
        });
    }

    if plans.is_empty() {
        return false;
    }

    let mut to_remove: Vec<u64> = Vec::new();
    let mut toast_msgs: Vec<String> = Vec::new();
    for plan in &plans {
        let n = plan.older_ids.len();
        // Subsume each older item's state into the survivor, then drop them.
        // Snapshot the older items (immutable borrow) before mutating the
        // survivor (mutable borrow) -- the queue can't be borrowed both ways
        // at once.
        let older_snaps: Vec<QueueItem> = {
            let q = state.queue.lock().await;
            plan.older_ids
                .iter()
                .filter_map(|id| q.get(*id).cloned())
                .collect()
        };
        {
            let mut q = state.queue.lock().await;
            if let Some(survivor) = q.get_mut(plan.survivor_id) {
                for older in &older_snaps {
                    subsume(survivor, older);
                }
            }
        }
        for &old_id in &plan.older_ids {
            to_remove.push(old_id);
        }
        let label = if plan.filename.is_empty() {
            format!("item {}", plan.survivor_id)
        } else {
            plan.filename.clone()
        };
        toast_msgs.push(format!(
            "De-duplicated \u{201c}{label}\u{201d}: kept the latest, merged {n} older duplicate(s)."
        ));
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
        tracing::info!("dedupe: subsumed {n} duplicate Done item(s) into the newest");
        // Surface the de-duplication so it isn't buried in the download stack:
        // one toast per filename that was collapsed.
        for msg in &toast_msgs {
            state.emit(Event::Toast(render::render_ack(msg, false)));
        }
    }
    n > 0
}

/// Merge `from`'s state into `into` (the survivor) for any field `into` is
/// missing, so a de-duplicated older item's probed media / generated
/// thumbnails / resolved url + title are not lost when it is dropped. The
/// survivor's own values always win (it is the newest -- the item the user
/// was watching); only blanks are filled from the older item. `logs` and
/// `error` are *not* subsumed: the survivor keeps its own download output and
/// (it is `Done`) has no error to inherit.
fn subsume(into: &mut QueueItem, from: &QueueItem) {
    if into.url.is_empty() && !from.url.is_empty() {
        into.url = from.url.clone();
    }
    if into.title.is_none() {
        into.title = from.title.clone();
    }
    if into.duration.is_none() {
        into.duration = from.duration;
    }
    if into.media.is_none() {
        into.media = from.media.clone();
    }
    if into.thumbnail.is_none() {
        into.thumbnail = from.thumbnail.clone();
    }
    if into.thumbnails.is_empty() {
        into.thumbnails = from.thumbnails.clone();
    }
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
    let present: std::collections::HashSet<String> =
        match crate::library::scan(&state.cfg.download_dir) {
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
        let media = match tokio::time::timeout(PROBE_TIMEOUT, media::probe(ffprobe, &path)).await {
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

/// Step 5: reset stale native thumbnail sets. A Done item whose
/// `thumbnails.len()` no longer equals `frame_count(duration)` had its frames
/// generated under a different formula or a different (since-reprobed)
/// duration, so the set is stale. Drop the native frames (clearing `thumbnails`)
/// and, if the primary `thumbnail` was one of those native frames (i.e. a
/// fallback -- not a fetched remote thumb), clear it too. Step 6 then reclaims
/// the orphaned cache files (shared content-addressed files survive while any
/// other live item still references them) and step 7 regenerates the correct
/// count from scratch.
///
/// Items with an unknown duration are skipped: `frame_count(None)` is just the
/// 1-frame floor, not a meaningful target, so we cannot tell whether an
/// existing set is stale (and would rather keep a richer gallery than degrade
/// it to a single frame). Returns true if any item was reset.
async fn reset_stale_thumbnail_counts(state: &Arc<AppState>) -> bool {
    let mut reset = 0u64;
    {
        let mut q = state.queue.lock().await;
        for item in q.items.iter_mut() {
            if item.status != ItemStatus::Done || item.thumbnails.is_empty() {
                continue;
            }
            let duration = item
                .duration
                .or_else(|| item.media.as_ref().and_then(|m| m.duration))
                .filter(|d| *d > 0.0);
            // Unknown duration: can't compute a meaningful expected count.
            let Some(d) = duration else { continue };
            let expected = thumb::frame_count(Some(d));
            if item.thumbnails.len() == expected {
                continue;
            }
            tracing::info!(
                "reset: item {} has {} native frame(s), expected {expected}; regenerating",
                item.id,
                item.thumbnails.len()
            );
            // If the primary is one of the native frames (a fallback), drop it
            // so step 7 re-picks from the regenerated set. A fetched remote
            // thumb (not in `thumbnails`) is left untouched.
            if item
                .thumbnail
                .as_deref()
                .is_some_and(|t| item.thumbnails.iter().any(|n| n == t))
            {
                item.thumbnail = None;
            }
            item.thumbnails.clear();
            reset += 1;
        }
    }
    if reset > 0 {
        tracing::info!("reset: {reset} item(s) with stale thumbnail counts");
    }
    reset > 0
}

/// Step 6: garbage-collect the thumbnail cache. Each state file owns its
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
            tracing::debug!(
                "gc: could not read cache dir {}: {e}",
                state.cfg.cache_dir.display()
            );
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

/// Step 7 (background): for each Done item with a present file but no native
/// frames, extract `frame_count(duration)` frames with ffmpeg into `thumbnails` (the
/// item-page gallery) and set the *primary* `thumbnail` as a fallback -- only
/// when no remote thumbnail was fetched (the remote thumb, if present, is the
/// preferred highest-quality primary). One spawned task processes items
/// sequentially (each item's frames are themselves sequential ffmpeg passes);
/// idempotent via content-addressing (a frame whose content is already
/// cached is reused, even across videos), so concurrent/overlapping runs only
/// fill gaps.
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
                    i.filename.clone().map(|f| {
                        (
                            i.id,
                            f,
                            i.duration
                                .or_else(|| i.media.as_ref().and_then(|m| m.duration)),
                        )
                    })
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
                // Targeted per-card swap: native thumbnail generation only
                // changes this one item (its gallery + possibly the primary
                // thumb shown on the card), so re-rendering the whole grid
                // would needlessly flash every card.
                let q = state.queue.lock().await;
                if let Some(html) = q.get(id).map(render::render_card) {
                    state.emit(Event::Card { id, html });
                }
                drop(q);
                state.persist().await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `subsume` fills the survivor's blanks from the older item but never
    /// overwrites values the survivor already has -- it is the newest (the
    /// item the user was watching), so its own url / title / media / thumbs
    /// win; only missing fields are inherited. `logs` and `error` are never
    /// subsumed.
    #[test]
    fn subsume_fills_blanks_without_overwriting() {
        let now = time::OffsetDateTime::now_utc();
        // Survivor: a freshly-finished re-download. It has its own url + a
        // log line, but no media / thumbnails yet (those are filled by the
        // later probe + thumbnail steps).
        let mut survivor = QueueItem::new(2, "https://u".into());
        survivor.status = ItemStatus::Done;
        survivor.filename = Some("x.mp4".into());
        survivor.enqueued_at = now + time::Duration::seconds(10);
        survivor.logs.push("survivor log".into());

        // Older: a previous Done item for the same file, already probed +
        // thumbnailed.
        let mut older = QueueItem::new(1, "https://u".into());
        older.status = ItemStatus::Done;
        older.filename = Some("x.mp4".into());
        older.title = Some("Title".into());
        older.duration = Some(120.0);
        older.media = Some(MediaInfo::default());
        older.thumbnail = Some("t.jpg".into());
        older.thumbnails = vec!["a.jpg".into(), "b.jpg".into()];
        older.enqueued_at = now;
        older.logs.push("older log".into());

        subsume(&mut survivor, &older);

        // Survivor keeps its own url (was non-empty).
        assert_eq!(survivor.url, "https://u");
        // Blanks filled from the older item.
        assert_eq!(survivor.title.as_deref(), Some("Title"));
        assert_eq!(survivor.duration, Some(120.0));
        assert!(survivor.media.is_some());
        assert_eq!(survivor.thumbnail.as_deref(), Some("t.jpg"));
        assert_eq!(survivor.thumbnails, vec!["a.jpg", "b.jpg"]);
        // Logs are NOT subsumed (survivor keeps its own download output).
        assert_eq!(survivor.logs, vec!["survivor log"]);
    }

    /// When the survivor has no url (e.g. an imported item that turned out
    /// to collide with a real download's state file), the older item's url is
    /// inherited so the merged item still links back to its source.
    #[test]
    fn subsume_inherits_url_when_survivor_lacks_one() {
        let mut survivor = QueueItem::new(5, String::new()); // imported
        survivor.status = ItemStatus::Done;
        survivor.filename = Some("x.mp4".into());

        let mut older = QueueItem::new(1, "https://u".into());
        older.status = ItemStatus::Done;
        older.filename = Some("x.mp4".into());

        subsume(&mut survivor, &older);
        assert_eq!(survivor.url, "https://u");
    }
}
