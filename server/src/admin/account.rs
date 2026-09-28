use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::admins::validate_password;
use super::session::{self, AdminSession};
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(change_password))
        .routes(routes!(list_sessions))
        .routes(routes!(delete_session))
}

#[derive(Deserialize, ToSchema)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct SessionView {
    id: i64,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
    ip: Option<String>,
    user_agent: Option<String>,
    /// True for the session of this request.
    #[sqlx(default)]
    current: bool,
}

/// Changes the own password and ends all other own sessions.
#[utoipa::path(post, path = "/account/password", tag = "account", request_body = ChangePassword, responses(
    (status = NO_CONTENT),
    (status = BAD_REQUEST, body = ErrorBody, description = "The current password is wrong or the new one is not valid"),
))]
async fn change_password(
    current: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<ChangePassword>,
) -> ApiResult<StatusCode> {
    validate_password(&req.new_password)?;
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM admins WHERE id = ?")
        .bind(current.admin_id)
        .fetch_one(&state.db)
        .await?;
    if !state.hasher.verify(Some(hash), req.current_password).await? {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "wrong_password",
            "The current password is not correct.",
        ));
    }
    let new_hash = state.hasher.hash(req.new_password).await?;
    sqlx::query("UPDATE admins SET password_hash = ? WHERE id = ?")
        .bind(new_hash)
        .bind(current.admin_id)
        .execute(&state.db)
        .await?;
    session::delete_all_of(&state.db, current.admin_id, Some(current.session_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/account/sessions", tag = "account", responses((status = OK, body = Vec<SessionView>)))]
async fn list_sessions(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<SessionView>>> {
    let now = now();
    let mut sessions: Vec<SessionView> = sqlx::query_as(
        "SELECT id, created_at, last_seen_at, expires_at, ip, user_agent FROM sessions
         WHERE admin_id = ? AND expires_at > ? AND last_seen_at > ? ORDER BY last_seen_at DESC",
    )
    .bind(current.admin_id)
    .bind(now)
    .bind(session::idle_cutoff(now))
    .fetch_all(&state.db)
    .await?;
    for s in &mut sessions {
        s.current = s.id == current.session_id;
    }
    Ok(Json(sessions))
}

#[utoipa::path(delete, path = "/account/sessions/{id}", tag = "account", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn delete_session(
    current: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM sessions WHERE id = ? AND admin_id = ?")
        .bind(id)
        .bind(current.admin_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::not_found("The session does not exist."));
    }
    Ok(StatusCode::NO_CONTENT)
}
