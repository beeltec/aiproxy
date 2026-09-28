use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::{self, AdminSession};
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;

const MIN_PASSWORD: usize = 12;
const MAX_PASSWORD: usize = 1024;
const MAX_USERNAME: usize = 64;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_admins, create_admin))
        .routes(routes!(update_admin, delete_admin))
        .routes(routes!(reset_password))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct AdminView {
    id: i64,
    username: String,
    disabled: bool,
    created_at: i64,
    last_login_at: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateAdmin {
    username: String,
    password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateAdmin {
    disabled: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct ResetPassword {
    password: String,
}

pub fn validate_username(username: &str) -> ApiResult<()> {
    let valid_chars = username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if username.is_empty() || username.len() > MAX_USERNAME || !valid_chars {
        return Err(ApiError::bad_request(
            "The username must have 1 to 64 characters: letters, digits, dot, underscore or hyphen.",
        ));
    }
    Ok(())
}

pub fn validate_password(password: &str) -> ApiResult<()> {
    let length = password.chars().count();
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&length) {
        return Err(ApiError::bad_request(format!(
            "The password must have {MIN_PASSWORD} to {MAX_PASSWORD} characters."
        )));
    }
    Ok(())
}

fn last_admin_error() -> ApiError {
    ApiError::conflict("At least one enabled admin must remain.")
}

#[utoipa::path(get, path = "/admins", tag = "admins", responses((status = OK, body = Vec<AdminView>)))]
async fn list_admins(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<AdminView>>> {
    let admins =
        sqlx::query_as("SELECT id, username, disabled, created_at, last_login_at FROM admins ORDER BY username")
            .fetch_all(&state.db)
            .await?;
    Ok(Json(admins))
}

#[utoipa::path(post, path = "/admins", tag = "admins", request_body = CreateAdmin, responses(
    (status = CREATED, body = AdminView),
    (status = CONFLICT, body = ErrorBody, description = "The username exists"),
))]
async fn create_admin(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<CreateAdmin>,
) -> ApiResult<(StatusCode, Json<AdminView>)> {
    validate_username(&req.username)?;
    validate_password(&req.password)?;
    let password_hash = state.hasher.hash(req.password).await?;
    let admin = sqlx::query_as(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, ?)
         RETURNING id, username, disabled, created_at, last_login_at",
    )
    .bind(&req.username)
    .bind(password_hash)
    .bind(now())
    .fetch_one(&state.db)
    .await
    .map_err(|err| match err {
        sqlx::Error::Database(db) if db.is_unique_violation() => ApiError::conflict("This username exists."),
        other => other.into(),
    })?;
    Ok((StatusCode::CREATED, Json(admin)))
}

/// Disables or enables an admin. Disabling ends all sessions of that admin.
#[utoipa::path(patch, path = "/admins/{id}", tag = "admins", request_body = UpdateAdmin, responses(
    (status = OK, body = AdminView),
    (status = NOT_FOUND, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody, description = "It would leave no enabled admin"),
))]
async fn update_admin(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAdmin>,
) -> ApiResult<Json<AdminView>> {
    // One statement, so the check and the change cannot race. Session revocation is in the
    // same transaction, so a disabled admin never keeps a session.
    let mut tx = state.db.begin().await?;
    let admin: Option<AdminView> = sqlx::query_as(
        "UPDATE admins SET disabled = ?1 WHERE id = ?2
             AND (?1 = 0 OR (SELECT COUNT(*) FROM admins WHERE disabled = 0 AND id != ?2) > 0)
         RETURNING id, username, disabled, created_at, last_login_at",
    )
    .bind(req.disabled)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(admin) = admin else {
        drop(tx);
        return Err(missing_or_last(&state, id).await);
    };
    if req.disabled {
        session::delete_all_of(&mut *tx, id, None).await?;
    }
    tx.commit().await?;
    Ok(Json(admin))
}

#[utoipa::path(delete, path = "/admins/{id}", tag = "admins", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody, description = "It would leave no enabled admin"),
))]
async fn delete_admin(_: AdminSession, State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let deleted = sqlx::query(
        "DELETE FROM admins WHERE id = ?1
             AND (SELECT COUNT(*) FROM admins WHERE disabled = 0 AND id != ?1) > 0",
    )
    .bind(id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(missing_or_last(&state, id).await);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Sets a new password and ends the sessions of that admin (for the own account: all other sessions).
#[utoipa::path(post, path = "/admins/{id}/password", tag = "admins", request_body = ResetPassword, responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn reset_password(
    current: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<ResetPassword>,
) -> ApiResult<StatusCode> {
    validate_password(&req.password)?;
    let password_hash = state.hasher.hash(req.password).await?;
    let keep = (id == current.admin_id).then_some(current.session_id);
    if !set_password(&state.db, id, &password_hash, None, keep).await? {
        return Err(ApiError::not_found("The admin does not exist."));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Sets a new password hash and ends the sessions of the admin, except `keep_session`, in one
/// transaction. With `expected_hash`, it changes the password only if the current hash is still
/// that one. Returns false when nothing was changed.
pub async fn set_password(
    db: &SqlitePool,
    admin_id: i64,
    new_hash: &str,
    expected_hash: Option<&str>,
    keep_session: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let mut tx = db.begin().await?;
    let updated =
        sqlx::query("UPDATE admins SET password_hash = ?1 WHERE id = ?2 AND (?3 IS NULL OR password_hash = ?3)")
            .bind(new_hash)
            .bind(admin_id)
            .bind(expected_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if updated == 0 {
        return Ok(false);
    }
    session::delete_all_of(&mut *tx, admin_id, keep_session).await?;
    tx.commit().await?;
    Ok(true)
}

async fn missing_or_last(state: &AppState, id: i64) -> ApiError {
    let exists: Result<Option<i64>, _> = sqlx::query_scalar("SELECT id FROM admins WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await;
    match exists {
        Ok(Some(_)) => last_admin_error(),
        Ok(None) => ApiError::not_found("The admin does not exist."),
        Err(err) => err.into(),
    }
}
