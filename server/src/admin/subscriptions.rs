//! ChatGPT subscriptions: link accounts, refresh plans, primary account and failover order.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::chatgpt::link::{self, FlowState};
use crate::chatgpt::models;
use crate::chatgpt::refresh::{self, Failure, Trigger};
use crate::chatgpt::scheduler::{self, AccountPlan};
use crate::chatgpt::usage;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::settings;
use crate::state::AppState;

const MAX_LABEL: usize = 100;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_accounts))
        .routes(routes!(update_account, delete_account))
        .routes(routes!(make_primary))
        .routes(routes!(set_failover_order))
        .routes(routes!(refresh_now))
        .routes(routes!(sync_models))
        .routes(routes!(start_device_link))
        .routes(routes!(start_pkce_link))
        .routes(routes!(link_status, cancel_link))
        .routes(routes!(complete_pkce_link))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct AccountView {
    id: i64,
    email: Option<String>,
    plan_type: Option<String>,
    label: Option<String>,
    /// `active` or `needs_relogin`.
    status: String,
    is_primary: bool,
    failover_enabled: bool,
    failover_order: i64,
    /// `inherit`, `custom` or `disabled`.
    refresh_mode: String,
    refresh_cron: Option<String>,
    last_refresh_at: i64,
    last_refresh_error: Option<String>,
    last_refresh_failed_at: Option<i64>,
    access_expires_at: Option<i64>,
    created_at: i64,
    models: i64,
    /// Time of the last good model list sync.
    models_last_sync_at: Option<i64>,
    models_last_error: Option<String>,
    /// A usage-limit error blocks the account until this time.
    limited_until: Option<i64>,
    /// Usage limits from the last backend answer (usually a 5-hour and a weekly window).
    primary_used_percent: Option<f64>,
    primary_window_minutes: Option<i64>,
    primary_reset_at: Option<i64>,
    secondary_used_percent: Option<f64>,
    secondary_window_minutes: Option<i64>,
    secondary_reset_at: Option<i64>,
    quota_updated_at: Option<i64>,
    /// Credits of the account, from the last usage poll.
    has_credits: Option<bool>,
    credits_unlimited: Option<bool>,
    credits_balance: Option<String>,
    /// Next scheduled refresh, computed from the plan.
    #[sqlx(default)]
    next_refresh_at: Option<i64>,
}

async fn load_accounts(state: &AppState, only: Option<i64>) -> ApiResult<Vec<AccountView>> {
    let mut accounts: Vec<AccountView> = sqlx::query_as(
        "SELECT a.id, a.email, a.plan_type, a.label, a.status, a.is_primary, a.failover_enabled, a.failover_order,
             a.refresh_mode, a.refresh_cron, a.last_refresh_at, a.last_refresh_error, a.last_refresh_failed_at,
             a.access_expires_at, a.created_at,
             (SELECT COUNT(*) FROM chatgpt_account_models m WHERE m.account_id = a.id) AS models,
             a.models_last_sync_at, a.models_last_error,
             a.limited_until, q.primary_used_percent, q.primary_window_minutes, q.primary_reset_at,
             q.secondary_used_percent, q.secondary_window_minutes, q.secondary_reset_at,
             q.updated_at AS quota_updated_at, a.has_credits, a.credits_unlimited, a.credits_balance
         FROM chatgpt_accounts a LEFT JOIN chatgpt_quota q ON q.account_id = a.id
         WHERE ?1 IS NULL OR a.id = ?1
         ORDER BY a.is_primary DESC, a.failover_order, a.id",
    )
    .bind(only)
    .fetch_all(&state.db)
    .await?;
    let settings = settings::load(&state.db).await?;
    let now = Utc::now();
    for account in &mut accounts {
        let plan = AccountPlan {
            id: account.id,
            refresh_mode: account.refresh_mode.clone(),
            refresh_cron: account.refresh_cron.clone(),
            status: account.status.clone(),
        };
        account.next_refresh_at = scheduler::next_refresh(&settings, &plan, now).map(|at| at.timestamp());
    }
    Ok(accounts)
}

async fn load_account(state: &AppState, id: i64) -> ApiResult<AccountView> {
    load_accounts(state, Some(id))
        .await?
        .pop()
        .ok_or_else(|| ApiError::not_found("The account does not exist."))
}

#[utoipa::path(get, path = "/chatgpt/accounts", tag = "subscriptions", responses((status = OK, body = Vec<AccountView>)))]
async fn list_accounts(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<AccountView>>> {
    Ok(Json(load_accounts(&state, None).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct AccountUpdate {
    label: Option<String>,
    /// `inherit`, `custom` or `disabled`.
    refresh_mode: String,
    /// Needed when `refresh_mode` is `custom`.
    refresh_cron: Option<String>,
    /// Take part in failover.
    failover_enabled: bool,
}

#[utoipa::path(patch, path = "/chatgpt/accounts/{id}", tag = "subscriptions", request_body = AccountUpdate, responses(
    (status = OK, body = AccountView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn update_account(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<AccountUpdate>,
) -> ApiResult<Json<AccountView>> {
    let label = req.label.as_deref().map(str::trim).filter(|l| !l.is_empty());
    if label.is_some_and(|l| l.chars().count() > MAX_LABEL) {
        return Err(ApiError::bad_request("The label must have at most 100 characters."));
    }
    let cron = match req.refresh_mode.as_str() {
        "inherit" | "disabled" => None,
        "custom" => {
            let cron = req.refresh_cron.as_deref().map(str::trim).unwrap_or_default();
            settings::parse_cron(cron).map_err(ApiError::bad_request)?;
            Some(cron.to_owned())
        }
        _ => {
            return Err(ApiError::bad_request(
                "The refresh mode must be inherit, custom or disabled.",
            ));
        }
    };
    let updated = sqlx::query(
        "UPDATE chatgpt_accounts SET label = ?, refresh_mode = ?, refresh_cron = ?, failover_enabled = ? WHERE id = ?",
    )
    .bind(label)
    .bind(&req.refresh_mode)
    .bind(cron)
    .bind(req.failover_enabled)
    .bind(id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The account does not exist."));
    }
    state.schedule_changed.notify_one();
    Ok(Json(load_account(&state, id).await?))
}

#[utoipa::path(post, path = "/chatgpt/accounts/{id}/primary", tag = "subscriptions", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn make_primary(_: AdminSession, State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let updated = sqlx::query(
        "UPDATE chatgpt_accounts SET is_primary = (id = ?1) WHERE EXISTS (SELECT 1 FROM chatgpt_accounts WHERE id = ?1)",
    )
    .bind(id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("The account does not exist."));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
pub struct FailoverOrder {
    /// All account ids in the new order.
    account_ids: Vec<i64>,
}

#[utoipa::path(put, path = "/chatgpt/failover-order", tag = "subscriptions", request_body = FailoverOrder, responses(
    (status = NO_CONTENT),
))]
async fn set_failover_order(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<FailoverOrder>,
) -> ApiResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    for (position, id) in req.account_ids.iter().enumerate() {
        sqlx::query("UPDATE chatgpt_accounts SET failover_order = ? WHERE id = ?")
            .bind(position as i64 + 1)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Refreshes the token of the account. Then loads its model list and its usage. Only a failed
/// token refresh fails the request.
#[utoipa::path(post, path = "/chatgpt/accounts/{id}/refresh", tag = "subscriptions", responses(
    (status = OK, body = AccountView),
    (status = BAD_GATEWAY, body = ErrorBody, description = "The token refresh failed"),
))]
async fn refresh_now(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<AccountView>> {
    load_account(&state, id).await?;
    match refresh::refresh(&state, id, Trigger::Manual).await {
        Ok(()) => {
            // The model sync and the usage poll log their errors. The model sync also stores its error.
            let _ = tokio::join!(models::sync_account(&state, id, true), usage::refresh(&state, id));
            Ok(Json(load_account(&state, id).await?))
        }
        Err(err @ (Failure::NeedsRelogin(_) | Failure::Temporary(_))) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "refresh_failed",
            format!("The refresh failed: {err}"),
        )),
    }
}

#[utoipa::path(post, path = "/chatgpt/accounts/{id}/models/sync", tag = "subscriptions", responses(
    (status = OK, body = AccountView),
    (status = NOT_FOUND, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody, description = "The account must be linked again"),
    (status = BAD_GATEWAY, body = ErrorBody, description = "The model list sync failed"),
))]
async fn sync_models(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<AccountView>> {
    if load_account(&state, id).await?.status == "needs_relogin" {
        return Err(ApiError::conflict("Link the account again before you load its models."));
    }
    models::sync_account(&state, id, true).await.map_err(|err| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "model_sync_failed",
            format!("The model list sync failed: {err:#}"),
        )
    })?;
    Ok(Json(load_account(&state, id).await?))
}

/// Removes the account. If it was the primary account, the next one in the order becomes primary.
#[utoipa::path(delete, path = "/chatgpt/accounts/{id}", tag = "subscriptions", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn delete_account(_: AdminSession, State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let _lock = state.refresher.lock(id).await;
    let mut tx = state.db.begin().await?;
    let deleted = sqlx::query("DELETE FROM chatgpt_accounts WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::not_found("The account does not exist."));
    }
    sqlx::query(
        "UPDATE chatgpt_accounts SET is_primary = 1
         WHERE NOT EXISTS (SELECT 1 FROM chatgpt_accounts WHERE is_primary = 1)
             AND id = (SELECT id FROM chatgpt_accounts ORDER BY failover_order, id LIMIT 1)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    state.schedule_changed.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Link flows

#[derive(Serialize, ToSchema)]
pub struct DeviceLink {
    flow: String,
    user_code: String,
    verification_url: String,
}

#[derive(Serialize, ToSchema)]
pub struct PkceLink {
    flow: String,
    /// Open this address, log in, then paste the address of the page you land on.
    authorize_url: String,
}

#[derive(Serialize, ToSchema)]
pub struct LinkStatus {
    /// `pending`, `done` or `failed`.
    status: String,
    account_id: Option<i64>,
    message: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct PastedCallback {
    url: String,
}

fn link_error(message: String) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, "link_failed", message)
}

fn status_of(state: FlowState) -> LinkStatus {
    let (status, account_id, message) = match state {
        FlowState::Device | FlowState::Pkce { .. } | FlowState::Completing | FlowState::Saving => {
            ("pending", None, None)
        }
        FlowState::Done { account } => ("done", Some(account), None),
        FlowState::Failed { message } => ("failed", None, Some(message)),
    };
    LinkStatus {
        status: status.into(),
        account_id,
        message,
    }
}

#[utoipa::path(post, path = "/chatgpt/link/device", tag = "subscriptions", responses(
    (status = OK, body = DeviceLink),
    (status = BAD_GATEWAY, body = ErrorBody),
))]
async fn start_device_link(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<DeviceLink>> {
    let start = link::start_device(&state, current.session_id)
        .await
        .map_err(link_error)?;
    Ok(Json(DeviceLink {
        flow: start.flow,
        user_code: start.user_code,
        verification_url: start.verification_url.into(),
    }))
}

#[utoipa::path(post, path = "/chatgpt/link/pkce", tag = "subscriptions", responses((status = OK, body = PkceLink)))]
async fn start_pkce_link(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<PkceLink>> {
    let start = link::start_pkce(&state, current.session_id).map_err(ApiError::too_many_requests)?;
    Ok(Json(PkceLink {
        flow: start.flow,
        authorize_url: start.authorize_url,
    }))
}

#[utoipa::path(get, path = "/chatgpt/link/{flow}", tag = "subscriptions", responses(
    (status = OK, body = LinkStatus),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn link_status(
    current: AdminSession,
    State(state): State<AppState>,
    Path(flow): Path<String>,
) -> ApiResult<Json<LinkStatus>> {
    let flow = state
        .link_flows
        .get(&flow, current.session_id)
        .ok_or_else(|| ApiError::not_found("This sign-in expired. Start again."))?;
    Ok(Json(status_of(flow)))
}

#[utoipa::path(delete, path = "/chatgpt/link/{flow}", tag = "subscriptions", responses((status = NO_CONTENT)))]
async fn cancel_link(current: AdminSession, State(state): State<AppState>, Path(flow): Path<String>) -> StatusCode {
    state.link_flows.cancel(&flow, current.session_id);
    StatusCode::NO_CONTENT
}

#[utoipa::path(post, path = "/chatgpt/link/{flow}/callback", tag = "subscriptions", request_body = PastedCallback, responses(
    (status = OK, body = LinkStatus),
))]
async fn complete_pkce_link(
    current: AdminSession,
    State(state): State<AppState>,
    Path(flow): Path<String>,
    Json(req): Json<PastedCallback>,
) -> Json<LinkStatus> {
    Json(status_of(
        link::complete_pkce(&state, &flow, current.session_id, &req.url).await,
    ))
}
