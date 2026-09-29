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
    #[schema(value_type = Capabilities)]
    capabilities: Value,
    /// Admin changes. They win over the synced capabilities.
    #[sqlx(json)]
    #[schema(value_type = CapabilityOverrides)]
    capability_overrides: Value,
    /// The capabilities that the gateway uses.
    #[sqlx(skip)]
    #[schema(value_type = Capabilities)]
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

/// What a model can do. Every key is optional; a missing key means "unknown". The keys are
/// described in `connections::models`.
#[derive(Serialize, ToSchema)]
pub struct Capabilities {
    input: Option<Vec<String>>,
    efforts: Option<Vec<String>>,
    default_effort: Option<String>,
    fast: Option<bool>,
    context_window: Option<i64>,
    max_output: Option<i64>,
    endpoints: Option<Vec<String>>,
    chat_tools: Option<bool>,
    mode: Option<String>,
    thinking: Option<Thinking>,
    forced_tools_with_thinking: Option<bool>,
    thinking_always_on: Option<bool>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Thinking {
    adaptive: bool,
    enabled: bool,
}

/// The capabilities that an admin can change.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilityOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    efforts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fast: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoints: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forced_tools_with_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_always_on: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct ModelUpdate {
    enabled: bool,
    /// Capabilities to change. An empty object removes all changes.
    capability_overrides: CapabilityOverrides,
}

const INPUT_KINDS: [&str; 5] = ["text", "image", "file", "audio", "video"];
const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

fn check_overrides(overrides: &CapabilityOverrides) -> Result<(), String> {
    let known =
        |list: &Option<Vec<String>>, allowed: &[&str]| list.iter().flatten().all(|v| allowed.contains(&v.as_str()));
    if !known(&overrides.input, &INPUT_KINDS) {
        return Err("The input kinds must be text, image, file, audio or video.".into());
    }
    if !known(&overrides.efforts, &EFFORTS) {
        return Err(format!("The efforts must be from: {}.", EFFORTS.join(", ")));
    }
    if !known(&overrides.endpoints, &["chat", "responses"]) {
        return Err("The endpoints must be chat or responses.".into());
    }
    if [overrides.context_window, overrides.max_output]
        .iter()
        .flatten()
        .any(|v| *v < 1)
    {
        return Err("Token limits must be at least 1.".into());
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
    let overrides = serde_json::to_string(&req.capability_overrides).map_err(|_| ApiError::internal())?;
    let updated = sqlx::query("UPDATE models SET enabled = ?, capability_overrides = ? WHERE id = ?")
        .bind(req.enabled)
        .bind(overrides)
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
