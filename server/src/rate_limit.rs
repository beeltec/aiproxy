use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Counts events per key in a sliding time window. Memory is bounded by `MAX_KEYS`.
pub struct SlidingWindow {
    window: Duration,
    limit: usize,
    events: Mutex<HashMap<String, VecDeque<Instant>>>,
}

const MAX_KEYS: usize = 10_000;

impl SlidingWindow {
    pub fn new(window: Duration, limit: usize) -> Self {
        Self {
            window,
            limit,
            events: Mutex::new(HashMap::new()),
        }
    }

    /// True when the key has reached the limit in the current window.
    pub fn is_full(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut events = self.events.lock().expect("rate limit lock");
        events.get_mut(key).is_some_and(|list| {
            prune(list, now, self.window);
            list.len() >= self.limit
        })
    }

    /// Records an event. Returns false (and records nothing) when the limit is already reached.
    pub fn try_record(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut events = self.events.lock().expect("rate limit lock");
        if !events.contains_key(key) && events.len() >= MAX_KEYS {
            events.retain(|_, list| {
                prune(list, now, self.window);
                !list.is_empty()
            });
            if events.len() >= MAX_KEYS {
                let oldest = events
                    .iter()
                    .min_by_key(|(_, list)| list.back().copied())
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    events.remove(&oldest);
                }
            }
        }
        let list = events.entry(key.to_owned()).or_default();
        prune(list, now, self.window);
        if list.len() >= self.limit {
            return false;
        }
        list.push_back(now);
        true
    }

    pub fn clear(&self, key: &str) {
        self.events.lock().expect("rate limit lock").remove(key);
    }
}

fn prune(list: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while list.front().is_some_and(|t| now.duration_since(*t) >= window) {
        list.pop_front();
    }
}
