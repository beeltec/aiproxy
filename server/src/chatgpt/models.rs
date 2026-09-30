//! The model list of each ChatGPT account.

use std::time::Duration;

use anyhow::bail;
use serde::Deserialize;
use serde_json::{Value, json};

use super::refresh::{self, SendFailure};
use super::{accounts, backend, codex_version};
use crate::db::now;
use crate::gateway::codex::{self, SendError};
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_CHARS: usize = 300;

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

/// A model that was never seen before is enabled when the backend lists it. Known models keep
/// their `enabled` value.
pub async fn sync_account(state: &AppState, account: i64, force_version: bool) -> anyhow::Result<usize> {
    // One sync at a time, so an older list cannot replace a newer one.
    let _running = state.model_sync.lock().await;
    let result = async {
        let list = fetch(state, account, force_version).await?;
        save(state, account, &list).await
    }
    .await;
    if let Err(err) = &result {
        let error: String = format!("{err:#}").chars().take(MAX_ERROR_CHARS).collect();
        sqlx::query("UPDATE chatgpt_accounts SET models_last_error = ? WHERE id = ?")
            .bind(error)
            .bind(account)
            .execute(&state.db)
            .await?;
    }
    result
}

async fn fetch(state: &AppState, account: i64, force_version: bool) -> anyhow::Result<ModelsResponse> {
    let installation_id = backend::installation_id(&state.db).await?;
    let client_version = codex_version::get(state, force_version).await;
    let sent = refresh::send_with_fresh_token(state, account, || {
        request(state, account, &installation_id, &client_version)
    });
    let response = match sent.await {
        Ok(response) => response,
        Err(SendFailure::Renew(failure)) => bail!("the token cannot be renewed: {failure}"),
        Err(SendFailure::Send(SendError::Unauthorized)) => bail!("the ChatGPT backend did not accept the token"),
        Err(SendFailure::Send(SendError::UsageLimit { message, .. } | SendError::Throttled { message, .. })) => {
            bail!("the model list request failed: {message}")
        }
        Err(SendFailure::Send(SendError::Failed { status, message })) => {
            bail!("the model list request failed with {status}: {message}")
        }
    };
    Ok(response.json().await?)
}

async fn save(state: &AppState, account: i64, list: &ModelsResponse) -> anyhow::Result<usize> {
    let now = now();
    // The delete comes first: it takes the write lock before any read in this transaction.
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM chatgpt_account_models WHERE account_id = ?")
        .bind(account)
        .execute(&mut *tx)
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
            "fast": tiers.iter().any(|tier| matches!(*tier, "priority" | "fast")),
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
        .bind(listed)
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
    sqlx::query("UPDATE chatgpt_accounts SET models_last_sync_at = ?, models_last_error = NULL WHERE id = ?")
        .bind(now)
        .bind(account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(account, models = list.models.len(), "ChatGPT model list loaded");
    Ok(list.models.len())
}

async fn request(
    state: &AppState,
    account: i64,
    installation_id: &str,
    client_version: &str,
) -> Result<reqwest::Response, SendError> {
    let credentials = accounts::credentials(state, account)
        .await
        .map_err(|err| SendError::Failed {
            status: 500,
            message: format!("cannot read the account tokens: {err}"),
        })?;
    let request = state
        .http
        .get(format!("{}/models", backend::BASE_URL))
        .query(&[("client_version", client_version)])
        .header("Accept", "application/json")
        .timeout(TIMEOUT);
    let response = backend::with_headers(request, &credentials, installation_id, client_version)
        .send()
        .await
        .map_err(|err| SendError::Failed {
            status: 502,
            message: format!("cannot reach the ChatGPT backend: {err}"),
        })?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = codex::error_text(response).await;
    Err(codex::classify(status.as_u16(), &text, None))
}
