use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Counts events per key in a sliding time window. When a key reaches the limit, it stays
/// blocked for one full window from that moment. Memory is bounded by `MAX_KEYS`.
pub struct SlidingWindow {
    window: Duration,
    limit: usize,
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Default)]
struct Entry {
    events: VecDeque<Instant>,
    blocked_since: Option<Instant>,
}

impl Entry {
    fn prune(&mut self, now: Instant, window: Duration) {
        while self.events.front().is_some_and(|t| now.duration_since(*t) >= window) {
            self.events.pop_front();
        }
        if self.blocked_since.is_some_and(|t| now.duration_since(t) >= window) {
            self.blocked_since = None;
        }
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.blocked_since.is_none()
    }

    fn last_seen(&self) -> Option<Instant> {
        self.events.back().copied().max(self.blocked_since)
    }
}

const MAX_KEYS: usize = 10_000;

impl SlidingWindow {
    pub fn new(window: Duration, limit: usize) -> Self {
        Self {
            window,
            limit,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// True when the key is blocked.
    pub fn is_full(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("rate limit lock");
        entries.get_mut(key).is_some_and(|entry| {
            entry.prune(now, self.window);
            entry.blocked_since.is_some()
        })
    }

    /// Records an event. Returns false (and records nothing) when the key is blocked.
    pub fn try_record(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("rate limit lock");
        if !entries.contains_key(key) && entries.len() >= MAX_KEYS {
            self.make_room(&mut entries, now);
        }
        let entry = entries.entry(key.to_owned()).or_default();
        entry.prune(now, self.window);
        if entry.blocked_since.is_some() {
            return false;
        }
        entry.events.push_back(now);
        if entry.events.len() >= self.limit {
            entry.blocked_since = Some(now);
        }
        true
    }

    pub fn clear(&self, key: &str) {
        self.entries.lock().expect("rate limit lock").remove(key);
    }

    fn make_room(&self, entries: &mut HashMap<String, Entry>, now: Instant) {
        entries.retain(|_, entry| {
            entry.prune(now, self.window);
            !entry.is_empty()
        });
        if entries.len() >= MAX_KEYS {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen())
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
    }
}
