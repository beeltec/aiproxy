//! Responses clients such as the AI SDK send earlier output items back as references by id
//! (`item_reference`). The upstreams store nothing, so the gateway keeps the output items of
//! Responses answers in memory, by API key and item id, and puts them back inline. Nothing is
//! stored on disk.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ENTRIES: usize = 10_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;

type Key = (i64, String);

#[derive(Default)]
struct Inner {
    entries: HashMap<Key, (Instant, Arc<Value>, usize)>,
    order: VecDeque<Key>,
    bytes: usize,
}

#[derive(Default)]
pub struct ItemCache(Mutex<Inner>);

impl ItemCache {
    /// Keeps each output item that has an id. Items larger than the entry limit are not kept.
    pub fn store(&self, api_key: i64, output: &[Value]) {
        let entries: Vec<(Key, Arc<Value>, usize)> = output
            .iter()
            .filter_map(|item| {
                let id = item["id"].as_str()?;
                let size = serde_json::to_vec(item).map_or(usize::MAX, |bytes| bytes.len());
                (size <= MAX_ENTRY_BYTES).then(|| ((api_key, id.to_owned()), Arc::new(item.clone()), size))
            })
            .collect();
        let mut inner = self.0.lock().expect("item cache lock");
        for (key, item, size) in entries {
            if let Some((_, _, old)) = inner.entries.insert(key.clone(), (Instant::now(), item, size)) {
                inner.bytes -= old;
            } else {
                inner.order.push_back(key);
            }
            inner.bytes += size;
            while inner.entries.len() > MAX_ENTRIES || inner.bytes > MAX_BYTES {
                let Some(oldest) = inner.order.pop_front() else { break };
                if let Some((_, _, size)) = inner.entries.remove(&oldest) {
                    inner.bytes -= size;
                }
            }
        }
    }

    /// The item and its size in bytes.
    pub fn get(&self, api_key: i64, id: &str) -> Option<(Arc<Value>, usize)> {
        let inner = self.0.lock().expect("item cache lock");
        inner
            .entries
            .get(&(api_key, id.to_owned()))
            .filter(|(at, _, _)| at.elapsed() < MAX_AGE)
            .map(|(_, item, size)| (item.clone(), *size))
    }
}
