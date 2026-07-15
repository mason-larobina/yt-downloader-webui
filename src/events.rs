//! Event types flowing from worker -> SSE clients, plus the ring buffer used to
//! replay recent log lines to freshly-connected tabs.
use std::collections::VecDeque;

/// An event broadcast to all connected `/events` clients. Payloads are
/// pre-rendered HTML fragments (server-side escaped).
///
/// The video-card grid is driven by **targeted** events rather than a single
/// blanket `Queue` re-render, so that updating one card never destroys the
/// rest of the grid's DOM (which would re-trigger the `.card-overlay`
/// opacity transition and drop `:hover` state on any card the user is
/// interacting with):
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
#[derive(Clone, Debug)]
pub enum Event {
    /// Replaces `#status` (the active-item progress bar / idle status).
    Status(String),
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
}

impl Event {
    /// The SSE event name used by the htmx `sse-swap` attribute. `Card`
    /// events use a per-id name (`card-<id>`) so each card listens only to
    /// its own updates.
    pub fn name(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Event::Status(_) => "status".into(),
            Event::Log(_) => "log".into(),
            Event::Queue(_) => "queue".into(),
            Event::Library(_) => "library".into(),
            Event::Card { id, .. } => format!("card-{id}").into(),
            Event::CardAdded(_) => "card-added".into(),
            Event::CardsCount(_) => "cards-count".into(),
        }
    }

    /// The HTML payload.
    pub fn data(&self) -> &str {
        match self {
            Event::Status(s)
            | Event::Log(s)
            | Event::Queue(s)
            | Event::Library(s)
            | Event::CardAdded(s)
            | Event::CardsCount(s) => s,
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
