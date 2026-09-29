//! The Pricing page: price lists, the prices of the enabled models, overrides and recompute.

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
use crate::prices::Prices;
use crate::prices::recompute::{self, RecomputeStatus};
use crate::prices::sync::{self, PriceSource};
use crate::state::AppState;

const MAX_KEY: usize = 200;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_sources))
        .routes(routes!(sync_now))
        .routes(routes!(model_prices))
        .routes(routes!(list_overrides, create_override))
        .routes(routes!(update_override, delete_override))
        .routes(routes!(recompute_status, start_recompute))
}

#[utoipa::path(get, path = "/pricing/sources", tag = "pricing", responses((status = OK, body = Vec<PriceSource>)))]
async fn list_sources(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<PriceSource>>> {
    Ok(Json(sync::sources(&state.db).await?))
}

/// Loads all price lists now. Returns when the sync is done.
#[utoipa::path(post, path = "/pricing/sync", tag = "pricing", responses((status = OK, body = Vec<PriceSource>)))]
async fn sync_now(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<PriceSource>>> {
    sync::sync(&state).await?;
    Ok(Json(sync::sources(&state.db).await?))
}

#[derive(Serialize, ToSchema)]
pub struct ModelPrice {
    /// `chatgpt/<model>` or `<connection slug>/<model>`.
    model: String,
    /// `chatgpt`, `openai`, `anthropic` or `openrouter`.
    kind: String,
    /// Empty when no price is known: the cost of the model is unknown.
    price: Option<PriceView>,
}

#[derive(Serialize, ToSchema)]
pub struct PriceView {
    version: i64,
    /// `override`, `openrouter`, `litellm` or `models_dev`.
    source: String,
    /// The override match key or the model key in the list.
    key: String,
    prices: Prices,
}

/// The prices that apply to each enabled model now.
#[utoipa::path(get, path = "/pricing/models", tag = "pricing", responses((status = OK, body = Vec<ModelPrice>)))]
async fn model_prices(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<ModelPrice>>> {
    let models: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT COALESCE(c.slug, 'chatgpt'), COALESCE(c.kind, 'chatgpt'), m.upstream_id
         FROM models m LEFT JOIN connections c ON m.source = CAST(c.id AS TEXT)
         WHERE m.enabled = 1 AND (m.source = 'chatgpt' OR c.id IS NOT NULL)
         ORDER BY m.source <> 'chatgpt', 1, 3",
    )
    .fetch_all(&state.db)
    .await?;
    let book = state.prices.get();
    let out = models
        .into_iter()
        .map(|(slug, kind, model)| {
            let name = format!("{slug}/{model}");
            // ChatGPT models have OpenAI API prices.
            let priced_kind = if kind == "chatgpt" { "openai" } else { kind.as_str() };
            let price = book
                .resolve(Some(&name), priced_kind, &model)
                .map(|resolved| PriceView {
                    version: resolved.version,
                    source: resolved.source,
                    key: resolved.key,
                    prices: (*resolved.prices).clone(),
                });
            ModelPrice {
                model: name,
                kind,
                price,
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct OverrideView {
    id: i64,
    /// `connection/model`, `chatgpt/model` or a model key such as `openai/gpt-5`.
    match_key: String,
    version_id: i64,
    #[sqlx(json)]
    prices: Prices,
    created_by: Option<String>,
    created_at: i64,
}

#[derive(Deserialize, ToSchema)]
pub struct OverrideSettings {
    match_key: String,
    prices: Prices,
}

#[derive(Deserialize, ToSchema)]
pub struct OverridePrices {
    prices: Prices,
}

async fn overrides(state: &AppState, only: Option<i64>) -> ApiResult<Vec<OverrideView>> {
    Ok(sqlx::query_as(
        "SELECT o.id, o.match_key, o.version_id, v.prices, a.username AS created_by, o.created_at
         FROM price_overrides o JOIN price_versions v ON v.id = o.version_id
         LEFT JOIN admins a ON a.id = o.created_by
         WHERE o.active = 1 AND (?1 IS NULL OR o.id = ?1) ORDER BY o.match_key",
    )
    .bind(only)
    .fetch_all(&state.db)
    .await?)
}

async fn override_view(state: &AppState, id: i64) -> ApiResult<OverrideView> {
    overrides(state, Some(id))
        .await?
        .pop()
        .ok_or_else(|| ApiError::not_found("The override does not exist."))
}

fn check_prices(prices: &Prices) -> ApiResult<String> {
    prices.validate().map_err(ApiError::bad_request)?;
    Ok(serde_json::to_string(prices).expect("prices serialize"))
}

async fn insert_version(tx: &mut sqlx::SqliteConnection, key: &str, prices: &str) -> Result<i64, sqlx::Error> {
    // Override versions are never current: the override points to its version.
    sqlx::query_scalar(
        "INSERT INTO price_versions (source, model_key, prices, created_at, current)
         VALUES ('override', ?, ?, ?, 0) RETURNING id",
    )
    .bind(key)
    .bind(prices)
    .bind(now())
    .fetch_one(tx)
    .await
}

#[utoipa::path(get, path = "/pricing/overrides", tag = "pricing", responses((status = OK, body = Vec<OverrideView>)))]
async fn list_overrides(_: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Vec<OverrideView>>> {
    Ok(Json(overrides(&state, None).await?))
}

/// New costs use the override at once; stored rows change only with a recompute.
#[utoipa::path(post, path = "/pricing/overrides", tag = "pricing", request_body = OverrideSettings, responses(
    (status = CREATED, body = OverrideView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody),
))]
async fn create_override(
    session: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<OverrideSettings>,
) -> ApiResult<(StatusCode, Json<OverrideView>)> {
    let key = req.match_key.trim();
    if key.is_empty() || key.len() > MAX_KEY || !key.contains('/') || key.contains(char::is_whitespace) {
        return Err(ApiError::bad_request(
            "The model must be a name such as `openai/gpt-5` or `chatgpt/gpt-5.5`.",
        ));
    }
    let prices = check_prices(&req.prices)?;
    let mut tx = state.db.begin().await?;
    let version = insert_version(&mut tx, key, &prices).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO price_overrides (match_key, active, version_id, created_by, created_at)
         VALUES (?, 1, ?, ?, ?) RETURNING id",
    )
    .bind(key)
    .bind(version)
    .bind(session.admin_id)
    .bind(now())
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::conflict("This model has an override. Edit that one.")
        }
        _ => err.into(),
    })?;
    tx.commit().await?;
    state.prices.reload(&state.db).await?;
    Ok((StatusCode::CREATED, Json(override_view(&state, id).await?)))
}

/// Saves the prices as a new version. Rows with the old version keep it until a recompute.
#[utoipa::path(put, path = "/pricing/overrides/{id}", tag = "pricing", request_body = OverridePrices, responses(
    (status = OK, body = OverrideView),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn update_override(
    _: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<OverridePrices>,
) -> ApiResult<Json<OverrideView>> {
    let prices = check_prices(&req.prices)?;
    let current = override_view(&state, id).await?;
    let mut tx = state.db.begin().await?;
    let version = insert_version(&mut tx, &current.match_key, &prices).await?;
    sqlx::query("UPDATE price_overrides SET version_id = ? WHERE id = ? AND active = 1")
        .bind(version)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.prices.reload(&state.db).await?;
    Ok(Json(override_view(&state, id).await?))
}

/// The override stops applying; its versions stay for the rows that use them.
#[utoipa::path(delete, path = "/pricing/overrides/{id}", tag = "pricing", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn delete_override(_: AdminSession, State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let changed = sqlx::query("UPDATE price_overrides SET active = 0 WHERE id = ? AND active = 1")
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(ApiError::not_found("The override does not exist."));
    }
    state.prices.reload(&state.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
pub struct RecomputeRequest {
    /// Unix seconds, inclusive.
    from: i64,
    /// Unix seconds, exclusive.
    to: i64,
    /// Resolved model names (`connection/model`); empty: all models.
    #[serde(default)]
    models: Vec<String>,
}

#[utoipa::path(get, path = "/pricing/recompute", tag = "pricing", responses((status = OK, body = RecomputeStatus)))]
async fn recompute_status(_: AdminSession, State(state): State<AppState>) -> Json<RecomputeStatus> {
    Json(state.recompute.status())
}

/// Recalculates the cost of the rows in the time range with the current prices.
#[utoipa::path(post, path = "/pricing/recompute", tag = "pricing", request_body = RecomputeRequest, responses(
    (status = ACCEPTED, body = RecomputeStatus),
    (status = BAD_REQUEST, body = ErrorBody),
    (status = CONFLICT, body = ErrorBody),
))]
async fn start_recompute(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<RecomputeRequest>,
) -> ApiResult<(StatusCode, Json<RecomputeStatus>)> {
    if req.from >= req.to {
        return Err(ApiError::bad_request("The start must be before the end."));
    }
    if req.models.len() > 100 || req.models.iter().any(|m| m.len() > MAX_KEY) {
        return Err(ApiError::bad_request("Select at most 100 models."));
    }
    if !recompute::start(&state, req.from, req.to, req.models) {
        return Err(ApiError::conflict("A recompute is running. Wait until it ends."));
    }
    Ok((StatusCode::ACCEPTED, Json(state.recompute.status())))
}
