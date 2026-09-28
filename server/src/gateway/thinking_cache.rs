//! Chat clients cannot keep signed Anthropic thinking blocks, but Anthropic needs them back in
//! tool loops. The gateway keeps the complete assistant content of such turns in memory, by API
//! key and tool call id, and puts it back when the tool results come. Nothing is stored on disk.

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
    entries: HashMap<Key, (Instant, Arc<Vec<Value>>, usize)>,
    order: VecDeque<Key>,
    bytes: usize,
}

#[derive(Default)]
pub struct ThinkingCache(Mutex<Inner>);

impl ThinkingCache {
    /// Keeps the content for each tool call id in it. Content without thinking is not kept.
    pub fn store(&self, api_key: i64, content: &[Value]) {
        let has_thinking = content
            .iter()
            .any(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")));
        let ids: Vec<String> = content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["id"].as_str().map(str::to_owned))
            .collect();
        let size = serde_json::to_vec(content).map_or(usize::MAX, |bytes| bytes.len());
        if !has_thinking || ids.is_empty() || size > MAX_ENTRY_BYTES {
            return;
        }
        let content = Arc::new(content.to_vec());
        let mut inner = self.0.lock().expect("thinking cache lock");
        for id in ids {
            let key = (api_key, id);
            if let Some((_, _, old)) = inner
                .entries
                .insert(key.clone(), (Instant::now(), content.clone(), size))
            {
                inner.bytes -= old;
            } else {
                inner.order.push_back(key);
            }
            inner.bytes += size;
        }
        while inner.entries.len() > MAX_ENTRIES || inner.bytes > MAX_BYTES {
            let Some(oldest) = inner.order.pop_front() else { break };
            if let Some((_, _, size)) = inner.entries.remove(&oldest) {
                inner.bytes -= size;
            }
        }
    }

    pub fn get(&self, api_key: i64, tool_call_id: &str) -> Option<Arc<Vec<Value>>> {
        let inner = self.0.lock().expect("thinking cache lock");
        inner
            .entries
            .get(&(api_key, tool_call_id.to_owned()))
            .filter(|(at, _, _)| at.elapsed() < MAX_AGE)
            .map(|(_, content, _)| content.clone())
    }
}
