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
    let installation_id = backend::installation_id(&state.db).await?;
    let client_version = codex_version::get(state, false).await;
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

/// One model list request with the current credentials.
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
