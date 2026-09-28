//! The OpenAI- and Anthropic-compatible endpoints below `/v1`.

mod auth;
mod error;
mod rejected;

use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::{Json, Router, middleware};
use serde_json::{Value, json};

pub use auth::KeyLimits;
pub use rejected::RejectedCounter;

use crate::state::AppState;

pub const PREFIX: &str = "/v1";
/// Images and audio can be large.
const MAX_BODY: usize = 64 * 1024 * 1024;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/models", get(list_models))
        .layer(middleware::from_fn_with_state(state, auth::authenticate))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// Model list. It stays empty until providers and routing exist.
async fn list_models() -> Json<Value> {
    Json(json!({ "object": "list", "data": [] }))
}
