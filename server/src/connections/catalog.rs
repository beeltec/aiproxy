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
pub struct CatalogCache(Mutex<Option<(Instant, Arc<Catalog>)>>);

/// The current catalog. When a download fails, the last good catalog (or an empty one) is used.
pub async fn get(state: &AppState) -> Arc<Catalog> {
    let cached = state.catalog.0.lock().expect("catalog lock").clone();
    if let Some((loaded, catalog)) = &cached
        && loaded.elapsed() < MAX_AGE
    {
        return catalog.clone();
    }
    match load(&state.http).await {
        Ok(catalog) => {
            let catalog = Arc::new(catalog);
            *state.catalog.0.lock().expect("catalog lock") = Some((Instant::now(), catalog.clone()));
            catalog
        }
        Err(err) => {
            tracing::warn!(error = %err, "cannot load the model catalogs");
            cached.map(|(_, catalog)| catalog).unwrap_or_default()
        }
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let response = http.get(url).timeout(TIMEOUT).send().await?.error_for_status()?;
    Ok(response.json().await?)
}

async fn load(http: &reqwest::Client) -> anyhow::Result<Catalog> {
    let (models_dev, litellm) = tokio::try_join!(fetch(http, MODELS_DEV), fetch(http, LITELLM))?;
    let mut catalog = Catalog::default();
    for provider in ["openai", "anthropic"] {
        let models = models_dev[provider]["models"].as_object().into_iter().flatten();
        for (id, entry) in models {
            catalog
                .models_dev
                .insert((provider.to_owned(), id.clone()), entry.clone());
        }
    }
    for (id, entry) in litellm.as_object().into_iter().flatten() {
        if let Some(provider @ ("openai" | "anthropic")) = entry["litellm_provider"].as_str() {
            catalog.litellm.insert((provider.to_owned(), id.clone()), entry.clone());
        }
    }
    Ok(catalog)
}
