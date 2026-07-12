//! Core in-memory server state: `AppState`, `Queue`, `QueueItem`, `Progress`.
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::{Event, RingBuffer};

/// Capacity of the broadcast channel (events buffered per lagging receiver).
pub const EVENT_CHANNEL_CAP: usize = 512;
/// Max log lines replayed on connect.
pub const LOG_RING_CAP: usize = 1000;
/// Max items retained in the queue (terminal history). Pending + active always retained.
pub const QUEUE_HISTORY_CAP: usize = 200;

/// Shared application state.
pub struct AppState {
    pub cfg: Config,
    pub queue: Mutex<Queue>,
    pub events: broadcast::Sender<Event>,
    pub log_ring: Mutex<RingBuffer<String>>,
    pub notify: Notify,
    /// Global shutdown token. Also wired into each active item's cancel select!.
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn new(cfg: Config, queue: Queue) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        Arc::new(AppState {
            cfg,
            queue: Mutex::new(queue),
            events,
            log_ring: Mutex::new(RingBuffer::new(LOG_RING_CAP)),
            notify: Notify::new(),
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

    /// Append a pending **probe** item for a submitted URL of unknown type.
    /// The worker runs `--flat-playlist -j` on it first; a playlist is
    /// expanded into per-video items, a single video is downloaded directly.
    /// Returns the new item's id.
    pub fn enqueue(&mut self, url: String) -> u64 {
        let id = self.alloc_id();
        let item = QueueItem::new(id, url);
        self.items.push(item);
        id
    }

    /// Append a pending **video** item produced by playlist expansion. These
    /// are known single-video URLs (`entry.url` from `--flat-playlist -j` is
    /// already the full watch URL), so the worker downloads them directly
    /// without re-probing. Returns the new item's id.
    pub fn enqueue_video(
        &mut self,
        url: String,
        title: Option<String>,
        duration: Option<f64>,
    ) -> u64 {
        let id = self.alloc_id();
        let mut item = QueueItem::new(id, url);
        item.kind = ItemKind::Video;
        item.title = title;
        item.duration = duration;
        self.items.push(item);
        id
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
    /// Drop all terminal (Done/Failed/Cancelled) items. Pending + active retained.
    /// Returns the number removed.
    pub fn clear_terminal(&mut self) -> usize {
        let before = self.items.len();
        self.items.retain(|i| match i.status {
            ItemStatus::Done | ItemStatus::Failed | ItemStatus::Cancelled => false,
            _ => true,
        });
        before - self.items.len()
    }

    /// Trim terminal history so the queue view stays bounded.
    pub fn trim_history(&mut self) {
        if self.items.len() <= QUEUE_HISTORY_CAP {
            return;
        }
        // Keep all non-terminal items; trim oldest terminal ones.
        let mut terminal_count = self
            .items
            .iter()
            .filter(|i| i.status.is_terminal())
            .count();
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
    /// How the worker treats this item: `Probe` (submitted, classify first)
    /// or `Video` (known single video, download directly). See `ItemKind`.
    pub kind: ItemKind,
    /// Video title resolved from the `--flat-playlist -j` probe, used as the
    /// row label before yt-dlp emits a `Destination:`/filename. `None` for
    /// submitted URLs until they are probed.
    pub title: Option<String>,
    /// Duration in seconds from the probe, for display. `None` if unknown.
    pub duration: Option<f64>,
    pub filename: Option<String>,
    pub progress: Option<Progress>,
    pub error: Option<String>,
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
            kind: ItemKind::Probe,
            title: None,
            duration: None,
            filename: None,
            progress: None,
            error: None,
            cancel: None,
            enqueued_at: time::OffsetDateTime::now_utc(),
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

/// How the worker should treat a queued item.
///
/// - `Probe`: a *submitted* URL of unknown type. The worker first runs
///   `yt-dlp --flat-playlist -j` to classify it: a playlist (`_type:"url"`
///   entries) is expanded into per-video `Video` items; a single video
///   (`_type:"video"` / no entries) is then downloaded directly. This costs a
///   second extraction for single videos but needs no URL-shape guessing.
/// - `Video`: a known single-video URL (produced by expansion). The worker
///   downloads it directly -- no probe -- since its type is already known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Probe,
    Video,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Probe => "probe",
            ItemKind::Video => "video",
        }
    }

    /// Default for persisted items written before `kind` existed: treat as a
    /// fresh submitted URL (re-probe on restart). Safe because probing a
    /// single video just downloads it, and probing a playlist re-expands.
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "video" => ItemKind::Video,
            _ => ItemKind::Probe,
        }
    }
}

impl Default for ItemKind {
    fn default() -> Self {
        ItemKind::Probe
    }
}

impl ItemStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, ItemStatus::Done | ItemStatus::Failed | ItemStatus::Cancelled)
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
    pub elapsed: Option<f64>,
    pub fragment_index: Option<u64>,
    pub fragment_count: Option<u64>,
    /// yt-dlp's numeric percentage (0..=100), e.g. `73.0`.
    pub percent: Option<f64>,
    /// yt-dlp's preformatted percentage string like " 73.0%".
    pub percent_str: Option<String>,
    /// yt-dlp's preformatted speed string.
    pub speed_str: Option<String>,
    pub eta_str: Option<String>,
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
