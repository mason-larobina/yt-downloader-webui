//! Core in-memory server state: `AppState`, `Queue`, `QueueItem`, `Progress`.
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::{Event, RingBuffer};
use crate::media::MediaInfo;

/// Capacity of the broadcast channel (events buffered per lagging receiver).
pub const EVENT_CHANNEL_CAP: usize = 512;
/// Max log lines replayed on connect.
pub const LOG_RING_CAP: usize = 1000;
/// Max items retained in the queue (terminal history). Pending + active always retained.
pub const QUEUE_HISTORY_CAP: usize = 200;
/// Max yt-dlp log lines retained per item (oldest dropped once exceeded).
/// Persisted alongside the item so logs survive a restart.
pub const ITEM_LOG_CAP: usize = 2000;

/// Shared application state.
pub struct AppState {
    pub cfg: Config,
    pub queue: Mutex<Queue>,
    pub events: broadcast::Sender<Event>,
    pub log_ring: Mutex<RingBuffer<String>>,
    pub notify: Notify,
    /// Reused async HTTP client for remote thumbnail fetches (HTTPS via rustls).
    /// The fetched thumbnail is the *primary* thumbnail (highest quality);
    /// when none is available, a native ffmpeg-extracted frame is the fallback.
    pub http: reqwest::Client,
    /// Global shutdown token. Also wired into each active item's cancel select!.
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn new(cfg: Config, queue: Queue) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let http = reqwest::Client::builder()
            .user_agent(concat!("yt-downloader-webui/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");
        Arc::new(AppState {
            cfg,
            queue: Mutex::new(queue),
            events,
            log_ring: Mutex::new(RingBuffer::new(LOG_RING_CAP)),
            notify: Notify::new(),
            http,
            shutdown: CancellationToken::new(),
        })
    }

    /// Broadcast an event to all connected SSE clients (no-op if none).
    pub fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }

    /// Persist the current queue to the state dir (one `<ts>.json` per item;
    /// clone-snapshot under lock, write outside the lock). Reconciles the dir
    /// to mirror the live queue: (re)writes each item's file and deletes
    /// orphaned files for cleared / trimmed items. Logs a warning on failure.
    pub async fn persist(&self) {
        let (next_id, items) = {
            let mut q = self.queue.lock().await;
            q.trim_history();
            (q.next_id, q.items.clone())
        };
        if let Err(e) = crate::persist::save(&self.cfg.state_dir, next_id, &items).await {
            tracing::warn!("failed to persist queue: {e:#}");
        }
    }
}

/// The global queue.
#[derive(Debug)]
pub struct Queue {
    pub items: Vec<QueueItem>,
    pub next_id: u64,
}

impl Queue {
    pub fn new() -> Self {
        Queue {
            items: Vec::new(),
            next_id: 1,
        }
    }

    /// Mint a fresh id.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Append a pending **video** item for a single-video URL. The worker
    /// downloads it directly (no probe). `title`/`duration` are borrowed from
    /// the synchronous probe in `POST /download` (a playlist entry's `url` is
    /// already the full watch URL; a single video keeps the original URL).
    /// Returns the new item's id, or `None` if the URL was skipped because a
    /// previously-downloaded item with the **exact same URL** already exists
    /// in the queue -- re-downloading a playlist should not create duplicate
    /// rows / files for videos already on disk.
    ///
    /// Playlists are never persisted: their expansion happens in the request
    /// handler and is presented for approval; only the approved per-video
    /// items reach the queue (and thus the state dir).
    pub fn enqueue(
        &mut self,
        url: String,
        title: Option<String>,
        duration: Option<f64>,
    ) -> Option<u64> {
        // Dedupe against successful downloads: if a Done item with the exact
        // same URL already exists, the video is already on disk -- skip it so
        // re-downloading a playlist doesn't create duplicate rows / files.
        // Failed/Cancelled items are intentionally NOT matched: those did not
        // produce a file and should be retried.
        if self
            .items
            .iter()
            .any(|i| i.url == url && i.status == ItemStatus::Done)
        {
            tracing::info!("skipping already-downloaded URL: {url}");
            return None;
        }
        let id = self.alloc_id();
        let mut item = QueueItem::new(id, url);
        item.title = title;
        item.duration = duration;
        self.items.push(item);
        Some(id)
    }

    /// Find an item by id.
    pub fn get(&self, id: u64) -> Option<&QueueItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Remove a pending item by id (never touches Active items).
    /// Returns true if removed.
    pub fn remove_pending(&mut self, id: u64) -> bool {
        if let Some(pos) = self
            .items
            .iter()
            .position(|i| i.id == id && i.status == ItemStatus::Pending)
        {
            self.items.remove(pos);
            return true;
        }
        false
    }
    /// Remove a terminal (Done/Failed/Cancelled) item by id. Returns true if
    /// removed. Pending/Active items are never removed (use cancel/retry).
    pub fn remove_terminal(&mut self, id: u64) -> bool {
        if let Some(pos) = self
            .items
            .iter()
            .position(|i| i.id == id && i.status.is_terminal())
        {
            self.items.remove(pos);
            return true;
        }
        false
    }
    /// Drop all terminal (Done/Failed/Cancelled) items. Pending + active retained.
    /// Returns the number removed.
    pub fn clear_terminal(&mut self) -> usize {
        let before = self.items.len();
        self.items.retain(|i| {
            !matches!(
                i.status,
                ItemStatus::Done | ItemStatus::Failed | ItemStatus::Cancelled
            )
        });
        before - self.items.len()
    }

    /// Trim terminal history so the queue view stays bounded.
    pub fn trim_history(&mut self) {
        if self.items.len() <= QUEUE_HISTORY_CAP {
            return;
        }
        // Keep all non-terminal items; trim oldest terminal ones.
        let mut terminal_count = self.items.iter().filter(|i| i.status.is_terminal()).count();
        let mut keep = Vec::with_capacity(self.items.len());
        for item in self.items.drain(..) {
            if item.status.is_terminal() && terminal_count > QUEUE_HISTORY_CAP / 2 {
                terminal_count -= 1;
                continue;
            }
            keep.push(item);
        }
        self.items = keep;
    }
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub id: u64,
    pub url: String,
    pub status: ItemStatus,
    /// Video title resolved from the `--flat-playlist -j` probe, used as the
    /// row label before yt-dlp emits a `Destination:`/filename. `None` for
    /// submitted URLs until they are probed.
    pub title: Option<String>,
    /// Duration in seconds from the probe, for display. `None` if unknown.
    pub duration: Option<f64>,
    pub filename: Option<String>,
    pub progress: Option<Progress>,
    pub error: Option<String>,
    /// Cache filename (bare basename, e.g. `<sha1>.jpg`) of the item's
    /// *primary* thumbnail in `cfg.cache_dir`. The primary is the remote
    /// thumbnail fetched from the probe's thumbnail URL (highest quality);
    /// when none is available, a native ffmpeg-extracted frame (the middle of
    /// the generated set in `thumbnails`) is used as a fallback. `None` until
    /// one resolves.
    pub thumbnail: Option<String>,
    /// All generated native thumbnail frames for this item (cache basenames
    /// like `<sha1>.0.jpg`, `<sha1>.1.jpg`, ...), produced by ffmpeg at
    /// logarithmically-spaced timestamps: `N = floor(ln(duration)) + 1`
    /// frames at `t = i/N * duration`. These populate the item-page gallery
    /// (a photo grid on the right pane) and act as the fallback `thumbnail`
    /// (primary) when no remote thumbnail was fetched. Empty until generated.
    pub thumbnails: Vec<String>,
    /// ffprobe-extracted media metadata for the on-disk file. Filled after a
    /// successful download and during import; persisted so we never re-probe
    /// the same file. `None` until probed.
    pub media: Option<MediaInfo>,
    /// Per-item yt-dlp output (stdout+stderr), captured line-by-line during
    /// the download and retained (capped to [`ITEM_LOG_CAP`]) for inspection in
    /// the logs pane at any point -- in-progress or after completion.
    /// Persisted with the item so the logs survive a server restart.
    pub logs: Vec<String>,
    /// Set when Active; POST /cancel and shutdown trip it.
    pub cancel: Option<CancellationToken>,
    pub enqueued_at: time::OffsetDateTime,
}

impl QueueItem {
    pub fn new(id: u64, url: String) -> Self {
        QueueItem {
            id,
            url,
            status: ItemStatus::Pending,
            title: None,
            duration: None,
            filename: None,
            progress: None,
            error: None,
            thumbnail: None,
            thumbnails: Vec::new(),
            media: None,
            logs: Vec::new(),
            cancel: None,
            enqueued_at: time::OffsetDateTime::now_utc(),
        }
    }

    /// Append one yt-dlp output line to this item's log buffer, trimming the
    /// oldest entries once [`ITEM_LOG_CAP`] is exceeded.
    pub fn push_log(&mut self, line: String) {
        self.logs.push(line);
        if self.logs.len() > ITEM_LOG_CAP {
            let drop_n = self.logs.len() - ITEM_LOG_CAP;
            self.logs.drain(0..drop_n);
        }
    }

    /// A short, human-readable label for the row. Preference: the on-disk
    /// filename (most accurate once yt-dlp picks one) > the probe-resolved
    /// title (good for pending per-video items) > the raw URL.
    pub fn label(&self) -> &str {
        self.filename
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or(&self.url)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    Pending,
    Active,
    Done,
    Failed,
    Cancelled,
}

impl ItemStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ItemStatus::Done | ItemStatus::Failed | ItemStatus::Cancelled
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Pending => "pending",
            ItemStatus::Active => "active",
            ItemStatus::Done => "done",
            ItemStatus::Failed => "failed",
            ItemStatus::Cancelled => "cancelled",
        }
    }
}

/// Subset of yt-dlp's progress JSON we render.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub status: Option<String>,
    pub filename: Option<String>,
    pub tmpfilename: Option<String>,
    pub downloaded_bytes: Option<f64>,
    pub total_bytes: Option<f64>,
    pub total_bytes_estimate: Option<f64>,
    pub speed: Option<f64>,
    pub eta: Option<f64>,
    /// yt-dlp's numeric percentage (0..=100), e.g. `73.0`.
    pub percent: Option<f64>,
    /// yt-dlp's preformatted percentage string like " 73.0%".
    pub percent_str: Option<String>,
}

impl Progress {
    /// 0.0..=100.0 if derivable from percent string or bytes.
    pub fn percent(&self) -> Option<f64> {
        if let Some(v) = self.percent {
            return Some(v.clamp(0.0, 100.0));
        }
        if let Some(s) = &self.percent_str {
            let t = s.trim().trim_end_matches('%');
            if let Ok(v) = t.parse::<f64>() {
                return Some(v.clamp(0.0, 100.0));
            }
        }
        let total = self
            .total_bytes
            .or(self.total_bytes_estimate)
            .filter(|t| *t > 0.0);
        if let (Some(downloaded), Some(total)) = (self.downloaded_bytes, total) {
            return Some((downloaded / total * 100.0).clamp(0.0, 100.0));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark_done(item: &mut QueueItem) {
        item.status = ItemStatus::Done;
    }

    /// Enqueueing a fresh URL returns a new id and appends a Pending item.
    #[test]
    fn enqueue_fresh_url() {
        let mut q = Queue::new();
        let id = q.enqueue("https://example/v/1".to_string(), None, None);
        assert_eq!(id, Some(1));
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].status, ItemStatus::Pending);
    }

    /// Re-enqueueing a URL whose previous item is Done returns None and does
    /// not append a row. This is the playlist-redownload dedupe regression:
    /// the same video must not be queued twice once it is already on disk.
    #[test]
    fn enqueue_skips_already_done() {
        let mut q = Queue::new();
        let id = q.enqueue("https://example/v/1".to_string(), None, None);
        assert_eq!(id, Some(1));
        mark_done(q.get_mut(1).unwrap());
        // Re-submit the exact same URL.
        let again = q.enqueue("https://example/v/1".to_string(), None, None);
        assert_eq!(again, None);
        assert_eq!(q.items.len(), 1, "no duplicate row created");
    }

    /// A Failed (or Cancelled) previous attempt is NOT a successful download,
    /// so re-enqueueing the same URL must still create a fresh pending item.
    #[test]
    fn enqueue_allows_retry_after_failure() {
        let mut q = Queue::new();
        let _ = q.enqueue("https://example/v/1".to_string(), None, None);
        q.get_mut(1).unwrap().status = ItemStatus::Failed;
        let again = q.enqueue("https://example/v/1".to_string(), None, None);
        assert_eq!(again, Some(2));
        assert_eq!(q.items.len(), 2);
    }

    /// Dedupe is exact-URL: a different URL is enqueued even when a Done
    /// item exists for the original.
    #[test]
    fn enqueue_dedupe_is_exact_url() {
        let mut q = Queue::new();
        let _ = q.enqueue("https://example/v/1".to_string(), None, None);
        mark_done(q.get_mut(1).unwrap());
        let other = q.enqueue("https://example/v/2".to_string(), None, None);
        assert_eq!(other, Some(2));
        assert_eq!(q.items.len(), 2);
    }

    /// A pending (in-flight) duplicate is not treated as a success: the user
    /// may legitimately re-submit a playlist; only Done gates the skip.
    #[test]
    fn enqueue_pending_is_not_skipped() {
        let mut q = Queue::new();
        let _ = q.enqueue("https://example/v/1".to_string(), None, None);
        // Still Pending.
        let again = q.enqueue("https://example/v/1".to_string(), None, None);
        assert_eq!(again, Some(2));
        assert_eq!(q.items.len(), 2);
    }
}
