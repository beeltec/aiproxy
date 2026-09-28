//! The OpenAI- and Anthropic-compatible endpoints below `/v1`.

mod auth;
mod chat_api;
mod codex;
mod engine;
mod error;
mod messages_api;
mod rejected;
mod request;
mod responses_api;
mod routing;
mod sse;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::{Value, json};

use auth::ApiKey;
use routing::Route;

pub use auth::KeyLimits;
pub use rejected::RejectedCounter;

use crate::state::AppState;

pub const PREFIX: &str = "/v1";
/// Images and audio can be large (the key check reads the body with the same limit).
const MAX_BODY: usize = 64 * 1024 * 1024;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/models", get(list_models))
        .route("/responses", post(responses_api::create))
        .route("/chat/completions", post(chat_api::create))
        .route("/messages", post(messages_api::create))
        .route("/messages/count_tokens", post(messages_api::count_tokens))
        .layer(middleware::from_fn_with_state(state, auth::authenticate))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// Enabled models that the key may use. Anthropic clients (with `anthropic-version`) get the
/// Anthropic list shape.
async fn list_models(State(state): State<AppState>, Extension(key): Extension<ApiKey>, headers: HeaderMap) -> Response {
    let rows: Vec<(i64, String, String, Option<String>, String)> = match sqlx::query_as(
        "SELECT id, source, upstream_id, display_name, capabilities FROM models WHERE enabled = 1
         ORDER BY source, upstream_id",
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "cannot list models");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let models: Vec<(String, String)> = rows
        .into_iter()
        .map(|(model_id, source, upstream_id, display_name, capabilities)| {
            let route = Route {
                requested: upstream_id.clone(),
                qualified: format!("{source}/{upstream_id}"),
                upstream_model: upstream_id.clone(),
                model_id,
                capabilities: serde_json::from_str(&capabilities).unwrap_or_default(),
            };
            (route, display_name.unwrap_or(upstream_id))
        })
        .filter(|(route, _)| routing::allowed(&key.allowlist, route))
        .map(|(route, name)| (route.qualified, name))
        .collect();

    if headers.contains_key("anthropic-version") {
        let data: Vec<Value> = models
            .iter()
            .map(|(id, name)| json!({ "type": "model", "id": id, "display_name": name, "created_at": "2026-01-01T00:00:00Z" }))
            .collect();
        return Json(json!({
            "data": data, "has_more": false,
            "first_id": models.first().map(|m| &m.0), "last_id": models.last().map(|m| &m.0),
        }))
        .into_response();
    }
    let data: Vec<Value> = models
        .iter()
        .map(|(id, _)| json!({ "id": id, "object": "model", "created": 0, "owned_by": id.split('/').next() }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}
