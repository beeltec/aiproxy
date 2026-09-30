//! The Codex CLI version that the proxy sends to the ChatGPT backend. The backend hides models
//! that need a newer client, so the version follows the latest release on npm.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::state::AppState;

/// The version to use until a lookup succeeds.
const FALLBACK: &str = "0.159.2";
const URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BYTES: usize = 1024 * 1024;
const MAX_VERSION_LEN: usize = 32;

#[derive(Clone)]
struct Cached {
    /// The last good version. `None` until a lookup succeeds.
    version: Option<String>,
    /// When the last lookup ended.
    loaded_at: Instant,
    next_load: Instant,
}

#[derive(Default)]
pub struct CodexVersionCache {
    cached: Mutex<Option<Cached>>,
    /// Only one lookup runs at a time.
    loading: tokio::sync::Mutex<()>,
    /// Set while a background lookup from `current` runs.
    refreshing: AtomicBool,
}

impl CodexVersionCache {
    fn cached(&self) -> Option<Cached> {
        self.cached.lock().expect("codex version lock").clone()
    }
}

/// The known version. It does not wait. When the cached value is too old, it starts one lookup
/// in the background.
pub fn current(state: &AppState) -> String {
    let cache = &state.codex_version;
    let cached = cache.cached();
    let due = cached.as_ref().is_none_or(|c| Instant::now() >= c.next_load);
    if due && !cache.refreshing.swap(true, Ordering::AcqRel) {
        let state = state.clone();
        tokio::spawn(async move {
            get(&state, false).await;
            state.codex_version.refreshing.store(false, Ordering::Release);
        });
    }
    version_of(cached.as_ref())
}

/// The version. It looks up npm when the cached value is too old, or always when `force` is set.
/// When the lookup fails, the last good version (or the built-in one) is used.
pub async fn get(state: &AppState, force: bool) -> String {
    let cache = &state.codex_version;
    let asked = Instant::now();
    if !force && let Some(cached) = cache.cached().filter(|c| asked < c.next_load) {
        return version_of(Some(&cached));
    }
    let _loading = cache.loading.lock().await;
    // A lookup that ended while this call waited gives the result.
    let cached = cache.cached();
    let done = cached.as_ref().filter(|c| {
        if force {
            c.loaded_at >= asked
        } else {
            Instant::now() < c.next_load
        }
    });
    if done.is_some() {
        return version_of(done);
    }
    let old = cached.and_then(|c| c.version);
    let (version, max_age) = match fetch(&state.http).await {
        Ok(version) => {
            tracing::debug!(%version, "loaded the Codex version");
            (Some(version), MAX_AGE)
        }
        Err(err) => {
            tracing::warn!(error = %err, "cannot load the Codex version, using the last known version");
            (old, RETRY_AFTER)
        }
    };
    let loaded_at = Instant::now();
    let cached = Cached {
        version,
        loaded_at,
        next_load: loaded_at + max_age,
    };
    let result = version_of(Some(&cached));
    *cache.cached.lock().expect("codex version lock") = Some(cached);
    result
}

fn version_of(cached: Option<&Cached>) -> String {
    cached
        .and_then(|c| c.version.clone())
        .unwrap_or_else(|| FALLBACK.to_owned())
}

#[derive(Deserialize)]
struct Package {
    version: String,
}

async fn fetch(http: &reqwest::Client) -> anyhow::Result<String> {
    use futures_util::StreamExt;
    let response = http.get(URL).timeout(TIMEOUT).send().await?.error_for_status()?;
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > MAX_BYTES {
            anyhow::bail!("the npm answer is larger than {} MB", MAX_BYTES / (1024 * 1024));
        }
    }
    let package: Package = serde_json::from_slice(&body)?;
    let parts: Vec<&str> = package.version.split('.').collect();
    let valid = package.version.len() <= MAX_VERSION_LEN
        && parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if !valid {
        anyhow::bail!("npm gives an unknown version format");
    }
    Ok(package.version)
}
