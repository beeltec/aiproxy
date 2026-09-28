//! Gateway API keys: create, edit, revoke.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::crypto::{random_bytes, sha256};
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;

pub const KEY_PREFIX: &str = "sk-aip-";
const KEY_RANDOM_CHARS: usize = 40;
const SHOWN_PREFIX_CHARS: usize = 12;
const MAX_NAME: usize = 100;
const MAX_PATTERNS: usize = 100;
const MAX_PATTERN: usize = 200;
const MAX_LIMIT: i64 = 1_000_000_000;
const MAX_CONCURRENCY: i64 = 1000;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_keys, create_key))
        .routes(routes!(update_key))
        .routes(routes!(revoke_key))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct ApiKeyView {
    id: i64,
    name: String,
    prefix: String,
    created_at: i64,
    created_by: Option<String>,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
    last_used_at: Option<i64>,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    /// Parallel requests. `None`: the default of 8.
    concurrency_limit: Option<i64>,
    #[sqlx(json)]
    allowlist: Vec<String>,
}

/// Settings of a key. The same body is used to create and to change a key.
#[derive(Deserialize, ToSchema)]
pub struct KeySettings {
    name: String,
    expires_at: Option<i64>,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    concurrency_limit: Option<i64>,
    /// Model patterns, for example `chatgpt/*` or `anthropic-main/claude-opus-5-5`. Empty: all models.
    allowlist: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedKey {
    /// The full key. It is shown only once.
    key: String,
    api_key: ApiKeyView,
}

macro_rules! select_keys {
    ($rest:literal) => {
        concat!(
            "SELECT k.id, k.name, k.prefix, k.created_at, a.username AS created_by, k.expires_at, k.revoked_at,
                 k.last_used_at, k.rpm_limit, k.tpm_limit, k.concurrency_limit, k.allowlist
             FROM api_keys k LEFT JOIN admins a ON a.id = k.created_by ",
            $rest
        )
    };
}

fn validate(settings: &KeySettings, creating: bool) -> ApiResult<()> {
    let name = settings.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(ApiError::bad_request("The name must have 1 to 100 characters."));
    }
    if creating && settings.expires_at.is_some_and(|at| at <= now()) {
        return Err(ApiError::bad_request("The expiry date must be in the future."));
    }
    for (value, max, label) in [
        (settings.rpm_limit, MAX_LIMIT, "requests per minute"),
        (settings.tpm_limit, MAX_LIMIT, "tokens per minute"),
        (settings.concurrency_limit, MAX_CONCURRENCY, "parallel requests"),
    ] {
        if value.is_some_and(|v| !(1..=max).contains(&v)) {
            return Err(ApiError::bad_request(format!(
                "The limit for {label} must be 1 to {max}."
            )));
        }
    }
    if settings.allowlist.len() > MAX_PATTERNS {
        return Err(ApiError::bad_request("Use at most 100 model patterns."));
    }
    for pattern in &settings.allowlist {
        let valid_chars = pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '*'));
        if pattern.is_empty() || pattern.len() > MAX_PATTERN || !valid_chars {
            return Err(ApiError::bad_request(format!(
                "`{pattern}` is not a valid model pattern. Use letters, digits, . _ - : / @ and * as wildcard."
            )));
        }
    }
    Ok(())
}

/// Random key with 40 characters from [A-Za-z0-9] (about 238 bits).
fn generate_key() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut key = String::from(KEY_PREFIX);
    while key.len() < KEY_PREFIX.len() + KEY_RANDOM_CHARS {
        // Bytes >= 248 are skipped, so every character is equally likely (248 = 4 * 62).
        for byte in random_bytes(64) {
            if byte < 248 && key.len() < KEY_PREFIX.len() + KEY_RANDOM_CHARS {
                key.push(ALPHABET[usize::from(byte) % ALPHABET.len()] as char);
            }
        }
    }
    key
}

fn allowlist_json(patterns: &[String]) -> String {
    serde_json::to_string(patterns).expect("a string list serializes")
}

#[utoipa::path(get, path = "/api-keys", tag = "api-keys", responses((status = OK, body = Vec<ApiKeyView>)))]
async fn list_keys(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<ApiKeyView>>> {
    let keys = sqlx::query_as(select_keys!("ORDER BY k.revoked_at IS NOT NULL, k.created_at DESC"))
        .fetch_all(&state.db)
        .await?;
    Ok(Json(keys))
}

#[utoipa::path(post, path = "/api-keys", tag = "api-keys", request_body = KeySettings, responses(
    (status = CREATED, body = CreatedKey),
    (status = BAD_REQUEST, body = ErrorBody),
))]
async fn create_key(
    current: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<KeySettings>,
) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    validate(&req, true)?;
    let key = generate_key();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys
             (name, prefix, key_hash, created_by, created_at, expires_at, rpm_limit, tpm_limit, concurrency_limit, allowlist)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(req.name.trim())
    .bind(&key[..SHOWN_PREFIX_CHARS])
    .bind(sha256(key.as_bytes()))
    .bind(current.admin_id)
    .bind(now())
    .bind(req.expires_at)
    .bind(req.rpm_limit)
    .bind(req.tpm_limit)
    .bind(req.concurrency_limit)
    .bind(allowlist_json(&req.allowlist))
    .fetch_one(&state.db)
    .await?;
    let api_key = sqlx::query_as(select_keys!("WHERE k.id = ?"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::CREATED, Json(CreatedKey { key, api_key })))
}

#[utoipa::path(put, path = "/api-keys/{id}", tag = "api-keys", request_body = KeySettings, responses(
    (status = OK, body = ApiKeyView),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn update_key(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<KeySettings>,
) -> ApiResult<Json<ApiKeyView>> {
    validate(&req, false)?;
    let updated = sqlx::query(
        "UPDATE api_keys SET name = ?, expires_at = ?, rpm_limit = ?, tpm_limit = ?, concurrency_limit = ?,
             allowlist = ?
         WHERE id = ?",
    )
    .bind(req.name.trim())
    .bind(req.expires_at)
    .bind(req.rpm_limit)
    .bind(req.tpm_limit)
    .bind(req.concurrency_limit)
    .bind(allowlist_json(&req.allowlist))
    .bind(id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The API key does not exist."));
    }
    let api_key = sqlx::query_as(select_keys!("WHERE k.id = ?"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(api_key))
}

/// Revokes a key. It stops working at once. Revoked keys stay listed for the usage history.
#[utoipa::path(post, path = "/api-keys/{id}/revoke", tag = "api-keys", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn revoke_key(_: AdminSession, State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let updated = sqlx::query("UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
        .bind(now())
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The API key does not exist."));
    }
    Ok(StatusCode::NO_CONTENT)
}
