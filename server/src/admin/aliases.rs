//! Aliases: model names for clients that point to a model and can set defaults.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::gateway::routing;
use crate::state::AppState;

const MAX_NAME: usize = 100;
const MAX_DESCRIPTION: usize = 500;
const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const SUMMARIES: [&str; 3] = ["auto", "concise", "detailed"];

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_aliases, create_alias))
        .routes(routes!(update_alias, delete_alias))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct AliasView {
    id: i64,
    name: String,
    /// `chatgpt/<model>` or `<connection slug>/<model>`.
    target: String,
    default_effort: Option<String>,
    default_fast: bool,
    /// `auto`, `concise` or `detailed`.
    default_summary: Option<String>,
    description: Option<String>,
    created_at: i64,
    /// False when the target model is missing or disabled: the alias does not work then.
    #[sqlx(default)]
    target_enabled: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct AliasSettings {
    name: String,
    target: String,
    default_effort: Option<String>,
    #[serde(default)]
    default_fast: bool,
    default_summary: Option<String>,
    description: Option<String>,
}

async fn views(state: &AppState, only: Option<i64>) -> ApiResult<Vec<AliasView>> {
    let mut aliases: Vec<AliasView> = sqlx::query_as(
        "SELECT id, name, target, default_effort, default_fast, default_summary, description, created_at
         FROM aliases WHERE ?1 IS NULL OR id = ?1 ORDER BY name",
    )
    .bind(only)
    .fetch_all(&state.db)
    .await?;
    for alias in &mut aliases {
        alias.target_enabled = matches!(routing::find_model(&state.db, &alias.target).await?, Some((_, true)));
    }
    Ok(aliases)
}

async fn view(state: &AppState, id: i64) -> ApiResult<AliasView> {
    views(state, Some(id))
        .await?
        .pop()
        .ok_or_else(|| ApiError::not_found("The alias does not exist."))
}

async fn validate(state: &AppState, req: &AliasSettings) -> ApiResult<()> {
    let name = req.name.trim();
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@'));
    if name.is_empty() || name.len() > MAX_NAME || !valid_chars {
        return Err(ApiError::bad_request(
            "The name must have 1 to 100 letters, digits or . _ - : @ (no `/`).",
        ));
    }
    if routing::find_model(&state.db, req.target.trim()).await?.is_none() {
        return Err(ApiError::bad_request(format!(
            "The model `{}` does not exist. Use a name from the Models tab.",
            req.target.trim()
        )));
    }
    if req.default_effort.as_deref().is_some_and(|e| !EFFORTS.contains(&e)) {
        return Err(ApiError::bad_request("The default effort is not valid."));
    }
    if req.default_summary.as_deref().is_some_and(|s| !SUMMARIES.contains(&s)) {
        return Err(ApiError::bad_request(
            "The default summary must be auto, concise or detailed.",
        ));
    }
    if req
        .description
        .as_deref()
        .is_some_and(|d| d.chars().count() > MAX_DESCRIPTION)
    {
        return Err(ApiError::bad_request(
            "The description must have at most 500 characters.",
        ));
    }
    Ok(())
}

fn conflict(err: sqlx::Error) -> ApiError {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => ApiError::conflict("An alias with this name exists."),
        _ => err.into(),
    }
}

fn description(req: &AliasSettings) -> Option<&str> {
    req.description.as_deref().map(str::trim).filter(|d| !d.is_empty())
}

#[utoipa::path(get, path = "/aliases", tag = "aliases", responses((status = OK, body = Vec<AliasView>)))]
async fn list_aliases(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<AliasView>>> {
    Ok(Json(views(&state, None).await?))
}

#[utoipa::path(post, path = "/aliases", tag = "aliases", request_body = AliasSettings, responses(
    (status = CREATED, body = AliasView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody),
))]
async fn create_alias(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<AliasSettings>,
) -> ApiResult<(StatusCode, Json<AliasView>)> {
    validate(&state, &req).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO aliases (name, target, default_effort, default_fast, default_summary, description, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(req.name.trim())
    .bind(req.target.trim())
    .bind(&req.default_effort)
    .bind(req.default_fast)
    .bind(&req.default_summary)
    .bind(description(&req))
    .bind(now())
    .fetch_one(&state.db)
    .await
    .map_err(conflict)?;
    Ok((StatusCode::CREATED, Json(view(&state, id).await?)))
}

#[utoipa::path(put, path = "/aliases/{id}", tag = "aliases", request_body = AliasSettings, responses(
    (status = OK, body = AliasView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn update_alias(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<AliasSettings>,
) -> ApiResult<Json<AliasView>> {
    validate(&state, &req).await?;
    let updated = sqlx::query(
        "UPDATE aliases SET name = ?, target = ?, default_effort = ?, default_fast = ?, default_summary = ?,
             description = ?
         WHERE id = ?",
    )
    .bind(req.name.trim())
    .bind(req.target.trim())
    .bind(&req.default_effort)
    .bind(req.default_fast)
    .bind(&req.default_summary)
    .bind(description(&req))
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(conflict)?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The alias does not exist."));
    }
    Ok(Json(view(&state, id).await?))
}

#[utoipa::path(delete, path = "/aliases/{id}", tag = "aliases", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn delete_alias(_: AdminSession, State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM aliases WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::not_found("The alias does not exist."));
    }
    Ok(StatusCode::NO_CONTENT)
}
