//! Event types flowing from worker -> SSE clients, plus the ring buffer used to
//! replay recent log lines to freshly-connected tabs.
use std::collections::VecDeque;

/// An event broadcast to all connected `/events` clients. Payloads are
/// pre-rendered HTML fragments (server-side escaped).
#[derive(Clone, Debug)]
pub enum Event {
    /// Replaces `#status` (the active-item progress bar / idle status).
    Status(String),
    /// Appended to `#log`.
    Log(String),
    /// Replaces `#queue` (the full queue list).
    Queue(String),
    /// Replaces `#library` (the file list).
    Library(String),
}

impl Event {
    /// The SSE event name used by the htmx `sse-swap` attribute.
    pub fn name(&self) -> &'static str {
        match self {
            Event::Status(_) => "status",
            Event::Log(_) => "log",
            Event::Queue(_) => "queue",
            Event::Library(_) => "library",
        }
    }

    /// The HTML payload.
    pub fn data(&self) -> &str {
        match self {
            Event::Status(s) | Event::Log(s) | Event::Queue(s) | Event::Library(s) => s,
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

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.deque.iter()
    }
    /// Drain all entries into a Vec (used for snapshot replay).
    pub fn snapshot(&self) -> Vec<&T> {
        self.deque.iter().collect()
    }
}
