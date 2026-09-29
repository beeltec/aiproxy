//! The Settings page: time zone, token refresh plan, failover, price sync plan.

use axum::Json;
use axum::extract::State;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::settings::{self, Settings};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_settings, put_settings))
        .routes(routes!(cron_preview))
}

#[utoipa::path(get, path = "/settings", tag = "settings", responses((status = OK, body = Settings)))]
async fn get_settings(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Settings>> {
    Ok(Json(settings::load(&state.db).await?))
}

#[utoipa::path(put, path = "/settings", tag = "settings", request_body = Settings, responses(
    (status = OK, body = Settings),
    (status = BAD_REQUEST, body = ErrorBody),
))]
async fn put_settings(
    _: AdminSession,
    State(state): State<AppState>,
    Json(mut req): Json<Settings>,
) -> ApiResult<Json<Settings>> {
    req.time_zone = req.time_zone.trim().to_owned();
    req.refresh.cron = req.refresh.cron.split_whitespace().collect::<Vec<_>>().join(" ");
    req.price_sync.cron = req.price_sync.cron.split_whitespace().collect::<Vec<_>>().join(" ");
    req.validate().map_err(ApiError::bad_request)?;
    settings::save(&state.db, &req).await?;
    state.schedule_changed.notify_one();
    state.price_schedule_changed.notify_one();
    Ok(Json(req))
}

#[derive(Deserialize, ToSchema)]
pub struct CronPreviewRequest {
    cron: String,
    /// Default: the instance time zone.
    time_zone: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CronPreview {
    /// The next 3 runs, unix seconds.
    next_runs: Vec<i64>,
}

/// The next 3 run times of a cron plan. The server computes them, so the dashboard shows what
/// the scheduler does.
#[utoipa::path(post, path = "/settings/cron-preview", tag = "settings", request_body = CronPreviewRequest, responses(
    (status = OK, body = CronPreview),
    (status = BAD_REQUEST, body = ErrorBody),
))]
async fn cron_preview(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<CronPreviewRequest>,
) -> ApiResult<Json<CronPreview>> {
    let tz = match req.time_zone.as_deref().map(str::trim).filter(|tz| !tz.is_empty()) {
        Some(name) => settings::time_zone(name).map_err(ApiError::bad_request)?,
        None => settings::load(&state.db).await?.tz(),
    };
    let cron = settings::parse_cron(req.cron.trim()).map_err(ApiError::bad_request)?;
    let next_runs = settings::next_runs(&cron, tz, Utc::now(), 3)
        .into_iter()
        .map(|at| at.timestamp())
        .collect();
    Ok(Json(CronPreview { next_runs }))
}
