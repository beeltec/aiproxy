//! The model list of each ChatGPT account.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{accounts, backend};
use crate::db::now;
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    slug: String,
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<Value>,
    #[serde(default)]
    service_tiers: Vec<Value>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    context_window: Option<i64>,
}

/// Loads the model list of an account and stores which models it can use. New models are
/// enabled only when this is the first ChatGPT model list; later ones wait for the admin.
pub async fn sync_account(state: &AppState, account: i64) -> anyhow::Result<usize> {
    let credentials = accounts::credentials(state, account).await?;
    let installation_id = backend::installation_id(&state.db).await?;
    let request = state
        .http
        .get(format!("{}/models", backend::BASE_URL))
        .query(&[("client_version", backend::CLIENT_VERSION)])
        .header("Accept", "application/json")
        .timeout(TIMEOUT);
    let response = backend::with_headers(request, &credentials, &installation_id)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body: String = response.text().await.unwrap_or_default().chars().take(300).collect();
        anyhow::bail!("the model list request failed with {status}: {body}");
    }
    let list: ModelsResponse = response.json().await?;

    let now = now();
    // The delete comes first: it takes the write lock before any read in this transaction.
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM chatgpt_account_models WHERE account_id = ?")
        .bind(account)
        .execute(&mut *tx)
        .await?;
    let first_list: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM models WHERE source = 'chatgpt')")
        .fetch_one(&mut *tx)
        .await?;
    for model in &list.models {
        let efforts: Vec<&str> = model
            .supported_reasoning_levels
            .iter()
            .filter_map(|level| level["effort"].as_str().or_else(|| level.as_str()))
            .collect();
        let tiers: Vec<&str> = model
            .service_tiers
            .iter()
            .filter_map(|tier| tier["id"].as_str().or_else(|| tier.as_str()))
            .collect();
        let capabilities = json!({
            "input": model.input_modalities,
            "efforts": efforts,
            "default_effort": model.default_reasoning_level,
            "service_tiers": tiers,
            "context_window": model.context_window,
            "visibility": model.visibility,
        });
        let listed = model.visibility.as_deref().is_none_or(|v| v == "list");
        let model_id: i64 = sqlx::query_scalar(
            "INSERT INTO models (source, upstream_id, display_name, enabled, capabilities, last_seen_at)
             VALUES ('chatgpt', ?, ?, ?, ?, ?)
             ON CONFLICT (source, upstream_id) DO UPDATE SET display_name = excluded.display_name,
                 capabilities = excluded.capabilities, last_seen_at = excluded.last_seen_at
             RETURNING id",
        )
        .bind(&model.slug)
        .bind(&model.display_name)
        .bind(first_list && listed)
        .bind(capabilities.to_string())
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO chatgpt_account_models (account_id, model_id, last_seen_at) VALUES (?, ?, ?)")
            .bind(account)
            .bind(model_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    tracing::info!(account, models = list.models.len(), "ChatGPT model list loaded");
    Ok(list.models.len())
}
