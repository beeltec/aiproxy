//! Keyed provider connections: add, edit, delete, load the model list.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use url::Url;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::connections::{Kind, key_aad, models};
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::outbound;
use crate::state::AppState;

const MAX_SLUG: usize = 40;
const MAX_NAME: usize = 100;
const MAX_KEY: usize = 1000;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_connections, create_connection))
        .routes(routes!(update_connection, delete_connection))
        .routes(routes!(sync_connection))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct ConnectionView {
    id: i64,
    /// The model prefix, for example `anthropic-main` in `anthropic-main/claude-opus-5-5`.
    slug: String,
    /// `openai`, `anthropic` or `openrouter`.
    kind: String,
    display_name: String,
    /// The last 4 characters of the API key.
    api_key_last4: String,
    base_url: Option<String>,
    created_at: i64,
    last_sync_at: Option<i64>,
    /// The error of the last model list request.
    last_error: Option<String>,
    models: i64,
    enabled_models: i64,
}

macro_rules! select_connections {
    ($rest:literal) => {
        concat!(
            "SELECT c.id, c.slug, c.kind, c.display_name, c.api_key_last4, c.base_url, c.created_at, c.last_sync_at,
                 c.last_error,
                 (SELECT COUNT(*) FROM models m WHERE m.source = CAST(c.id AS TEXT)) AS models,
                 (SELECT COUNT(*) FROM models m WHERE m.source = CAST(c.id AS TEXT) AND m.enabled) AS enabled_models
             FROM connections c ",
            $rest
        )
    };
}

async fn load(state: &AppState, id: i64) -> ApiResult<ConnectionView> {
    sqlx::query_as(select_connections!("WHERE c.id = ?"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| ApiError::not_found("The connection does not exist."))
}

#[utoipa::path(get, path = "/connections", tag = "connections", responses((status = OK, body = Vec<ConnectionView>)))]
async fn list_connections(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<ConnectionView>>> {
    let connections = sqlx::query_as(select_connections!("ORDER BY c.slug"))
        .fetch_all(&state.db)
        .await?;
    Ok(Json(connections))
}

#[derive(Deserialize, ToSchema)]
pub struct NewConnection {
    /// `openai`, `anthropic` or `openrouter`.
    kind: String,
    /// Lower-case letters, digits and `-`. It cannot change later.
    slug: String,
    display_name: String,
    api_key: String,
    /// Optional, for example a compatible gateway. Default: the official API.
    base_url: Option<String>,
}

fn check_name(name: &str) -> ApiResult<()> {
    if name.trim().is_empty() || name.trim().chars().count() > MAX_NAME {
        return Err(ApiError::bad_request("The name must have 1 to 100 characters."));
    }
    Ok(())
}

fn check_key(key: &str) -> ApiResult<()> {
    if key.trim().is_empty() || key.len() > MAX_KEY || key.trim().chars().any(char::is_whitespace) {
        return Err(ApiError::bad_request("The API key is not valid."));
    }
    Ok(())
}

/// A normalized base URL, or `None` for the default.
fn check_base_url(state: &AppState, base_url: Option<&str>) -> ApiResult<Option<String>> {
    let Some(raw) = base_url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let url = Url::parse(raw).map_err(|_| ApiError::bad_request("The base URL is not valid."))?;
    outbound::check_url(&url, state.config.allow_private_upstreams).map_err(ApiError::bad_request)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ApiError::bad_request("The base URL must not have a query or fragment."));
    }
    Ok(Some(url.as_str().trim_end_matches('/').to_owned()))
}

fn last4(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

#[utoipa::path(post, path = "/connections", tag = "connections", request_body = NewConnection, responses(
    (status = CREATED, body = ConnectionView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody),
))]
async fn create_connection(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<NewConnection>,
) -> ApiResult<(StatusCode, Json<ConnectionView>)> {
    let kind = Kind::parse(&req.kind)
        .ok_or_else(|| ApiError::bad_request("The kind must be openai, anthropic or openrouter."))?;
    let slug = req.slug.trim();
    let valid_slug = slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if slug.is_empty() || slug.len() > MAX_SLUG || !valid_slug || slug.starts_with('-') {
        return Err(ApiError::bad_request(
            "The slug must have 1 to 40 lower-case letters, digits or `-`.",
        ));
    }
    if slug == "chatgpt" {
        return Err(ApiError::bad_request("The slug `chatgpt` is reserved."));
    }
    check_name(&req.display_name)?;
    let api_key = req.api_key.trim();
    check_key(api_key)?;
    let base_url = check_base_url(&state, req.base_url.as_deref())?;

    // The key is encrypted with the row id, so the row is written first.
    let mut tx = state.db.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO connections (slug, kind, display_name, api_key_enc, api_key_last4, base_url, created_at)
         VALUES (?, ?, ?, x'', ?, ?, ?) RETURNING id",
    )
    .bind(slug)
    .bind(kind.as_str())
    .bind(req.display_name.trim())
    .bind(last4(api_key))
    .bind(&base_url)
    .bind(now())
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::conflict("A connection with this slug exists.")
        }
        _ => err.into(),
    })?;
    sqlx::query("UPDATE connections SET api_key_enc = ? WHERE id = ?")
        .bind(state.secrets.encrypt(&key_aad(id), api_key.as_bytes()))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // A failed model list is stored as `last_error`; the connection stays.
    if let Err(err) = models::sync(&state, id).await {
        tracing::warn!(connection = id, error = %err, "the first model list failed");
    }
    Ok((StatusCode::CREATED, Json(load(&state, id).await?)))
}

#[derive(Deserialize, ToSchema)]
pub struct ConnectionUpdate {
    display_name: String,
    /// A new key. Empty or missing: keep the key.
    api_key: Option<String>,
    base_url: Option<String>,
}

#[utoipa::path(patch, path = "/connections/{id}", tag = "connections", request_body = ConnectionUpdate, responses(
    (status = OK, body = ConnectionView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn update_connection(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<ConnectionUpdate>,
) -> ApiResult<Json<ConnectionView>> {
    check_name(&req.display_name)?;
    let base_url = check_base_url(&state, req.base_url.as_deref())?;
    let api_key = req.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if let Some(key) = api_key {
        check_key(key)?;
    }
    let updated = sqlx::query(
        "UPDATE connections SET display_name = ?, base_url = ?,
             api_key_enc = COALESCE(?, api_key_enc), api_key_last4 = COALESCE(?, api_key_last4)
         WHERE id = ?",
    )
    .bind(req.display_name.trim())
    .bind(&base_url)
    .bind(api_key.map(|key| state.secrets.encrypt(&key_aad(id), key.as_bytes())))
    .bind(api_key.map(last4))
    .bind(id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The connection does not exist."));
    }
    Ok(Json(load(&state, id).await?))
}

/// Deletes the connection and its models. Aliases that point to it stop working.
#[utoipa::path(delete, path = "/connections/{id}", tag = "connections", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn delete_connection(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let deleted = sqlx::query("DELETE FROM connections WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::not_found("The connection does not exist."));
    }
    sqlx::query("DELETE FROM models WHERE source = ?")
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Loads the model list again. New models start disabled.
#[utoipa::path(post, path = "/connections/{id}/sync", tag = "connections", responses(
    (status = OK, body = ConnectionView),
    (status = BAD_GATEWAY, body = ErrorBody),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn sync_connection(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<ConnectionView>> {
    load(&state, id).await?;
    models::sync(&state, id)
        .await
        .map_err(|err| ApiError::new(StatusCode::BAD_GATEWAY, "upstream_error", format!("{err:#}")))?;
    Ok(Json(load(&state, id).await?))
}
