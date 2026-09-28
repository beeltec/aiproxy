//! All models of the ChatGPT accounts and the connections: enable them and change capabilities.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::gateway::routing::effective_capabilities;
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_models))
        .routes(routes!(update_model))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct ModelView {
    id: i64,
    /// The name for clients, for example `chatgpt/gpt-5.5` or `or/openai/gpt-5.5`.
    name: String,
    /// `chatgpt`, or the slug of the connection.
    source: String,
    /// `chatgpt`, `openai`, `anthropic` or `openrouter`.
    source_kind: String,
    upstream_id: String,
    display_name: Option<String>,
    enabled: bool,
    /// The synced capabilities.
    #[sqlx(json)]
    #[schema(value_type = Object)]
    capabilities: Value,
    /// Admin changes. They win over the synced capabilities.
    #[sqlx(json)]
    #[schema(value_type = Object)]
    capability_overrides: Value,
    /// The capabilities that the gateway uses.
    #[sqlx(skip)]
    #[schema(value_type = Object)]
    effective: Value,
    last_seen_at: i64,
}

macro_rules! select_models {
    ($rest:literal) => {
        concat!(
            "SELECT m.id, COALESCE(c.slug, 'chatgpt') || '/' || m.upstream_id AS name,
                 COALESCE(c.slug, 'chatgpt') AS source, COALESCE(c.kind, 'chatgpt') AS source_kind,
                 m.upstream_id, m.display_name, m.enabled, m.capabilities, m.capability_overrides, m.last_seen_at
             FROM models m LEFT JOIN connections c ON m.source = CAST(c.id AS TEXT) ",
            $rest
        )
    };
}

fn with_effective(mut model: ModelView) -> ModelView {
    model.effective = effective_capabilities(&model.capabilities, &model.capability_overrides);
    model
}

#[utoipa::path(get, path = "/models", tag = "models", responses((status = OK, body = Vec<ModelView>)))]
async fn list_models(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<ModelView>>> {
    let models: Vec<ModelView> = sqlx::query_as(select_models!(
        "WHERE m.source = 'chatgpt' OR c.id IS NOT NULL ORDER BY m.source <> 'chatgpt', source, m.upstream_id"
    ))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(models.into_iter().map(with_effective).collect()))
}

#[derive(Deserialize, ToSchema)]
pub struct ModelUpdate {
    enabled: bool,
    /// Capability keys to change. An empty object removes all changes.
    #[schema(value_type = Object)]
    capability_overrides: Value,
}

const INPUT_KINDS: [&str; 5] = ["text", "image", "file", "audio", "video"];
const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

/// Only known keys with the right type are accepted.
fn check_overrides(overrides: &Value) -> Result<(), String> {
    let Some(map) = overrides.as_object() else {
        return Err("The capability changes must be an object.".into());
    };
    let list_of = |value: &Value, allowed: &[&str]| {
        value
            .as_array()
            .is_some_and(|list| list.iter().all(|v| v.as_str().is_some_and(|v| allowed.contains(&v))))
    };
    for (key, value) in map {
        let valid = match key.as_str() {
            "input" => list_of(value, &INPUT_KINDS),
            "efforts" => list_of(value, &EFFORTS),
            "endpoints" => list_of(value, &["chat", "responses"]),
            "fast" | "chat_tools" | "forced_tools_with_thinking" => value.is_boolean(),
            "context_window" | "max_output" => value.as_i64().is_some_and(|v| v > 0),
            "thinking" => value.as_object().is_some_and(|t| {
                t.iter()
                    .all(|(k, v)| matches!(k.as_str(), "adaptive" | "enabled") && v.is_boolean())
            }),
            _ => return Err(format!("`{key}` is not a capability that can be changed.")),
        };
        if !valid {
            return Err(format!("The value of `{key}` is not valid."));
        }
    }
    Ok(())
}

#[utoipa::path(put, path = "/models/{id}", tag = "models", request_body = ModelUpdate, responses(
    (status = OK, body = ModelView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn update_model(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<ModelUpdate>,
) -> ApiResult<Json<ModelView>> {
    check_overrides(&req.capability_overrides).map_err(ApiError::bad_request)?;
    let updated = sqlx::query("UPDATE models SET enabled = ?, capability_overrides = ? WHERE id = ?")
        .bind(req.enabled)
        .bind(req.capability_overrides.to_string())
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The model does not exist."));
    }
    let model = sqlx::query_as(select_models!("WHERE m.id = ?"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(with_effective(model)))
}
