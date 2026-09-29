//! Model metadata from the public catalogs models.dev and LiteLLM. It fills the capabilities
//! that a provider's own model list does not give. The catalogs load at most once a day.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Kind;
use crate::prices::sources::{LITELLM, MODELS_DEV};
use crate::state::AppState;

const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Entries per provider (`openai`, `anthropic`) and model id.
#[derive(Default)]
pub struct Catalog {
    models_dev: HashMap<(String, String), Value>,
    litellm: HashMap<(String, String), Value>,
}

impl Catalog {
    pub fn models_dev(&self, kind: Kind, model: &str) -> Option<&Value> {
        self.models_dev.get(&(kind.as_str().to_owned(), model.to_owned()))
    }

    pub fn litellm(&self, kind: Kind, model: &str) -> Option<&Value> {
        self.litellm.get(&(kind.as_str().to_owned(), model.to_owned()))
    }
}

#[derive(Clone)]
struct Cached {
    catalog: Arc<Catalog>,
    /// When the downloads of the catalog started.
    loaded_at: Instant,
    next_load: Instant,
}

#[derive(Default)]
pub struct CatalogCache(Mutex<Option<Cached>>);

impl CatalogCache {
    fn current(&self) -> Option<Cached> {
        self.0.lock().expect("catalog lock").clone()
    }

    /// Publishes a catalog, unless a catalog from later downloads is there already.
    fn publish(&self, catalog: Arc<Catalog>, loaded_at: Instant, complete: bool) -> Arc<Catalog> {
        let mut cached = self.0.lock().expect("catalog lock");
        if let Some(newer) = cached.as_ref().filter(|c| c.loaded_at > loaded_at) {
            return newer.catalog.clone();
        }
        // A failed source is tried again soon; the other source is used meanwhile.
        let next_load = Instant::now() + if complete { MAX_AGE } else { RETRY_AFTER };
        *cached = Some(Cached {
            catalog: catalog.clone(),
            loaded_at,
            next_load,
        });
        catalog
    }
}

/// The current catalog. When a download fails, the last good catalog (or an empty one) is used.
pub async fn get(state: &AppState) -> Arc<Catalog> {
    let cached = state.catalog.current();
    if let Some(cached) = cached.as_ref().filter(|c| Instant::now() < c.next_load) {
        return cached.catalog.clone();
    }
    let started = Instant::now();
    let old = cached.map(|c| c.catalog);
    let (catalog, complete) = load(&state.http, old.as_deref()).await;
    state.catalog.publish(Arc::new(catalog), started, complete)
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let response = http.get(url).timeout(TIMEOUT).send().await?.error_for_status()?;
    Ok(response.json().await?)
}

/// Loads both sources. A source that fails keeps its entries from `old`. Returns the catalog
/// and whether both sources loaded.
async fn load(http: &reqwest::Client, old: Option<&Catalog>) -> (Catalog, bool) {
    let (models_dev, litellm) = tokio::join!(fetch(http, MODELS_DEV), fetch(http, LITELLM));
    for (name, result) in [("models.dev", &models_dev), ("LiteLLM", &litellm)] {
        if let Err(err) = result {
            tracing::warn!(source = name, error = %err, "cannot load a model catalog");
        }
    }
    let complete = models_dev.is_ok() && litellm.is_ok();
    (build(models_dev.as_ref().ok(), litellm.as_ref().ok(), old), complete)
}

/// Replaces the catalog with lists that the price sync loaded. A missing list keeps its old
/// entries and is loaded again soon.
/// `loaded_at` is when the downloads started.
pub fn store(state: &AppState, models_dev: Option<&Value>, litellm: Option<&Value>, loaded_at: Instant) {
    let old = state.catalog.current().map(|c| c.catalog);
    let catalog = Arc::new(build(models_dev, litellm, old.as_deref()));
    let complete = models_dev.is_some() && litellm.is_some();
    state.catalog.publish(catalog, loaded_at, complete);
}

fn build(models_dev: Option<&Value>, litellm: Option<&Value>, old: Option<&Catalog>) -> Catalog {
    let mut catalog = Catalog::default();
    match models_dev {
        Some(models_dev) => {
            for provider in ["openai", "anthropic"] {
                let models = models_dev[provider]["models"].as_object().into_iter().flatten();
                for (id, entry) in models {
                    catalog
                        .models_dev
                        .insert((provider.to_owned(), id.clone()), entry.clone());
                }
            }
        }
        None => catalog.models_dev = old.map(|c| c.models_dev.clone()).unwrap_or_default(),
    }
    match litellm {
        Some(litellm) => {
            for (id, entry) in litellm.as_object().into_iter().flatten() {
                if let Some(provider @ ("openai" | "anthropic")) = entry["litellm_provider"].as_str() {
                    catalog.litellm.insert((provider.to_owned(), id.clone()), entry.clone());
                }
            }
        }
        None => catalog.litellm = old.map(|c| c.litellm.clone()).unwrap_or_default(),
    }
    catalog
}
