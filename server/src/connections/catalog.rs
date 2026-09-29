//! Model metadata from the public catalogs models.dev and LiteLLM. It fills the capabilities
//! that a provider's own model list does not give. The catalogs load at most once a day.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Kind;
use crate::state::AppState;

const MODELS_DEV: &str = "https://models.dev/api.json";
const LITELLM: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
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

#[derive(Default)]
/// The catalog and the time of its next load.
pub struct CatalogCache(Mutex<Option<(Instant, Arc<Catalog>)>>);

/// The current catalog. When a download fails, the last good catalog (or an empty one) is used.
pub async fn get(state: &AppState) -> Arc<Catalog> {
    let cached = state.catalog.0.lock().expect("catalog lock").clone();
    if let Some((next_load, catalog)) = &cached
        && Instant::now() < *next_load
    {
        return catalog.clone();
    }
    let old = cached.map(|(_, catalog)| catalog);
    let (catalog, complete) = load(&state.http, old.as_deref()).await;
    let catalog = Arc::new(catalog);
    // A failed source is tried again soon; the other source is used meanwhile.
    let next_load = Instant::now() + if complete { MAX_AGE } else { RETRY_AFTER };
    *state.catalog.0.lock().expect("catalog lock") = Some((next_load, catalog.clone()));
    catalog
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let response = http.get(url).timeout(TIMEOUT).send().await?.error_for_status()?;
    Ok(response.json().await?)
}

/// Loads both sources. A source that fails keeps its entries from `old`. Returns the catalog
/// and whether both sources loaded.
async fn load(http: &reqwest::Client, old: Option<&Catalog>) -> (Catalog, bool) {
    let (models_dev, litellm) = tokio::join!(fetch(http, MODELS_DEV), fetch(http, LITELLM));
    let mut catalog = Catalog::default();
    let complete = models_dev.is_ok() && litellm.is_ok();
    match models_dev {
        Ok(models_dev) => {
            for provider in ["openai", "anthropic"] {
                let models = models_dev[provider]["models"].as_object().into_iter().flatten();
                for (id, entry) in models {
                    catalog
                        .models_dev
                        .insert((provider.to_owned(), id.clone()), entry.clone());
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "cannot load the models.dev catalog");
            catalog.models_dev = old.map(|c| c.models_dev.clone()).unwrap_or_default();
        }
    }
    match litellm {
        Ok(litellm) => {
            for (id, entry) in litellm.as_object().into_iter().flatten() {
                if let Some(provider @ ("openai" | "anthropic")) = entry["litellm_provider"].as_str() {
                    catalog.litellm.insert((provider.to_owned(), id.clone()), entry.clone());
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "cannot load the LiteLLM catalog");
            catalog.litellm = old.map(|c| c.litellm.clone()).unwrap_or_default();
        }
    }
    (catalog, complete)
}
