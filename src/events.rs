//! Event types flowing from worker -> SSE clients, plus the ring buffer used to
//! replay recent log lines to freshly-connected tabs.
use std::collections::VecDeque;

/// An event broadcast to all connected `/events` clients. Payloads are
/// pre-rendered HTML fragments (server-side escaped).
///
/// Both the video-card grid **and** the floating `#status` banner are driven
/// by **targeted** events rather than a single blanket re-render, so updating
/// one part never destroys the surrounding DOM:
///
/// **Cards** -- re-rendering the whole grid would re-trigger every card's
/// `.card-overlay` opacity transition and drop `:hover` state:
/// - [`Event::Queue`] still replaces all of `#cards` `innerHTML`, but is now
///   emitted **only** on connect (snapshot) and on lag-recovery -- never on
///   the hot path. htmx reprocesses the swapped nodes, re-binding every
///   per-card `sse-swap` listener.
/// - [`Event::Card`] replaces exactly one card (its `outerHTML`); an empty
///   payload removes the card.
/// - [`Event::CardAdded`] prepends one newly-enqueued card to `#cards-list`
///   (`afterbegin`), so new/retried items appear at the top without touching
///   the rest of the grid.
/// - [`Event::CardsCount`] replaces just the `"N total, M pending"` count
///   text in the cards header.
///
/// **Status banner** -- the `#status` shell is a *stable* 3-column element
/// (left = thumbnail, middle = 4 lines: title / last log / progress bar /
/// progress text, right = cancel button) rendered once in `index.html` and
/// never swapped. Each slot is its own `sse-swap` target, so a progress tick
/// swaps only the bar + meta fragments -- never the `<img>` thumbnail, which
/// is what flashed when the whole `#status` `outerHTML` was swapped several
/// times a second:
/// - [`Event::StatusThumb`] / [`Event::StatusTitle`] / [`Event::StatusLog`] /
///   [`Event::StatusBar`] / [`Event::StatusMeta`] / [`Event::StatusCancel`]
///   each replace one slot's `innerHTML`. An empty payload collapses the slot
///   (CSS `:empty`); an empty `status-title` hides the whole banner (idle).
#[derive(Clone, Debug)]
pub enum Event {
    /// Appended to `#log`.
    Log(String),
    /// Replaces all of `#cards` (the full video-card list). Snapshot /
    /// lag-recovery only -- never the hot path (see the type docs above).
    Queue(String),
    /// Replaces `#library` (the file list).
    Library(String),
    /// Replaces one card (id-named `card-<id>` event -> that card's
    /// `outerHTML`). An empty `html` payload removes the card (the
    /// `outerHTML` swap of empty data deletes the node). Used for every
    /// in-place card change (status transition, filename/label, error,
    /// thumbnail landing) and for removals (cancel-pending, delete-item).
    Card { id: u64, html: String },
    /// Prepends one newly-enqueued (or retried) card to the top of
    /// `#cards-list` via an `afterbegin` swap on the `card-added` event.
    /// The card fragment carries its own `sse-swap="card-<id>"` listener,
    /// which htmx reprocesses on insert so it receives future `Card` events.
    CardAdded(String),
    /// Replaces the `"N total, M pending"` count text in the cards header
    /// (`cards-count` event -> `#cards-count` `innerHTML`).
    CardsCount(String),
    /// Replaces the banner's left-column thumbnail slot (`#status-thumb`
    /// `innerHTML`): an `<img>` once the thumbnail has landed, a placeholder
    /// `<div>` while an active download has no thumbnail yet, or empty
    /// (collapses the column) when nothing is active. Emitted on structural
    /// transitions (active item change) and on thumbnail landing -- never on
    /// the progress-tick hot path, so the `<img>` is not recreated several
    /// times a second.
    StatusThumb(String),
    /// Replaces the banner's first middle line (`#status-title` `innerHTML`):
    /// the active item's label + a `"N queued"` badge, a `"Waiting\u2026 N
    /// queued"` line when nothing is active but items are waiting, or empty
    /// (which hides the whole banner via CSS -- the idle state).
    StatusTitle(String),
    /// Replaces the banner's second middle line (`#status-log` `innerHTML`):
    /// the active item's most recent yt-dlp log line, or empty.
    StatusLog(String),
    /// Replaces the banner's progress-bar fill (`#status-bar` `innerHTML`):
    /// an `<i style="width:X%">` for the active download, or empty. Emitted
    /// on every throttled progress tick (the hot path).
    StatusBar(String),
    /// Replaces the banner's progress-text line (`#status-meta` `innerHTML`):
    /// the duration/ETA, percent, and speed/bytes spans for the active
    /// download, or empty. Emitted on every throttled progress tick.
    StatusMeta(String),
    /// Replaces the banner's right-column cancel button
    /// (`#status-cancel` `innerHTML`): a `<button hx-post="/cancel/<id>">` for
    /// the active download, or empty (collapses the column). Emitted only on
    /// structural transitions (active item change).
    StatusCancel(String),
}

impl Event {
    /// The SSE event name used by the htmx `sse-swap` attribute. `Card`
    /// events use a per-id name (`card-<id>`) so each card listens only to
    /// its own updates.
    pub fn name(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Event::Log(_) => "log".into(),
            Event::Queue(_) => "queue".into(),
            Event::Library(_) => "library".into(),
            Event::Card { id, .. } => format!("card-{id}").into(),
            Event::CardAdded(_) => "card-added".into(),
            Event::CardsCount(_) => "cards-count".into(),
            Event::StatusThumb(_) => "status-thumb".into(),
            Event::StatusTitle(_) => "status-title".into(),
            Event::StatusLog(_) => "status-log".into(),
            Event::StatusBar(_) => "status-bar".into(),
            Event::StatusMeta(_) => "status-meta".into(),
            Event::StatusCancel(_) => "status-cancel".into(),
        }
    }

    /// The HTML payload.
    pub fn data(&self) -> &str {
        match self {
            Event::Log(s)
            | Event::Queue(s)
            | Event::Library(s)
            | Event::CardAdded(s)
            | Event::CardsCount(s)
            | Event::StatusThumb(s)
            | Event::StatusTitle(s)
            | Event::StatusLog(s)
            | Event::StatusBar(s)
            | Event::StatusMeta(s)
            | Event::StatusCancel(s) => s,
            Event::Card { html, .. } => html,
        }
    }
}

/// A bounded FIFO buffer of strings; oldest entries are evicted when full.
/// Used to replay recent log lines on SSE connect.
#[derive(Debug)]
pub struct RingBuffer<T> {
    cap: usize,
    deque: VecDeque<T>,
}

impl<T> RingBuffer<T> {
    pub fn new(cap: usize) -> Self {
        RingBuffer {
            cap,
            deque: VecDeque::with_capacity(cap.min(64)),
        }
    }

    pub fn push(&mut self, item: T) {
        if self.cap == 0 {
            return;
        }
        if self.deque.len() >= self.cap {
            self.deque.pop_front();
        }
        self.deque.push_back(item);
    }

    /// Drain all entries into a Vec (used for snapshot replay).
    pub fn snapshot(&self) -> Vec<&T> {
        self.deque.iter().collect()
    }
}
