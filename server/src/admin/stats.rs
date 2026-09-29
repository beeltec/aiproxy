//! Usage statistics for the Overview and Usage pages.

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::session::AdminSession;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::prices::CATEGORIES;
use crate::settings;
use crate::state::AppState;

const MAX_BUCKETS: usize = 2000;
const MAX_FILTER: usize = 200;
/// The token categories: 8 input and 4 output (the first 12 of `CATEGORIES`).
const INPUTS: usize = 8;
const TOKENS: usize = 12;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(stats))
}

#[derive(Deserialize, ToSchema)]
pub struct StatsRequest {
    /// Unix seconds, inclusive.
    from: i64,
    /// Unix seconds, exclusive.
    to: i64,
    /// IANA time zone of the buckets, for example `Europe/Berlin`.
    time_zone: String,
    /// `hour`, `day`, `week` or `month`; empty: chosen by the length of the range.
    bucket: Option<String>,
    /// `key`, `model`, `upstream` or `none`.
    group: String,
    /// API key ids; empty: all keys.
    #[serde(default)]
    keys: Vec<i64>,
    /// Resolved model names; empty: all models.
    #[serde(default)]
    models: Vec<String>,
    /// Upstream group keys (`chatgpt:<account id>`, `connection:<id>`); empty: all.
    #[serde(default)]
    upstreams: Vec<String>,
}

/// Sums of usage rows. A request counts once, on its model row, so the groups add up to the
/// total.
#[derive(Serialize, ToSchema, Default)]
pub struct StatsTotals {
    requests: i64,
    /// Requests with an error status.
    errors: i64,
    /// Per category (the usage column names): tokens, calls, images, characters, seconds.
    amounts: BTreeMap<String, f64>,
    /// Calculated cost ("API value") per category, nano-USD.
    costs: BTreeMap<String, i64>,
    /// Calculated cost, nano-USD. Rows without a price are not in it.
    cost_nano: i64,
    /// The cost that providers reported (OpenRouter), nano-USD.
    reported_cost_nano: i64,
    /// Rows with usage but no price.
    unpriced: i64,
    /// Rows whose cost is not complete (estimated usage or missing prices).
    incomplete: i64,
}

/// One bucket of a group.
#[derive(Serialize, ToSchema, Default, Clone)]
pub struct StatsPoint {
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_nano: i64,
}

#[derive(Serialize, ToSchema)]
pub struct StatsGroup {
    /// The group value: API key id, model name, `chatgpt:<id>`, `connection:<id>` or `all`.
    key: String,
    label: String,
    totals: StatsTotals,
    /// One point per bucket.
    series: Vec<StatsPoint>,
}

#[derive(Serialize, ToSchema)]
pub struct StatsResponse {
    bucket: String,
    /// Bucket starts, unix seconds. The first bucket starts at `from`, the last ends at `to`.
    buckets: Vec<i64>,
    totals: StatsTotals,
    /// Requests that the gateway refused before routing (keys, limits), per reason.
    rejected: BTreeMap<String, i64>,
    /// Groups by calculated cost, highest first.
    groups: Vec<StatsGroup>,
}

/// The unix time of a local time. A local time in a clock change gap moves to the next hour.
fn local(tz: Tz, time: NaiveDateTime) -> i64 {
    match tz.from_local_datetime(&time) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t.timestamp(),
        LocalResult::None => local(tz, time + Duration::hours(1)),
    }
}

/// The bucket starts in `[from, to)`, with local borders in `tz`. The first bucket starts at
/// `from`.
fn buckets(from: i64, to: i64, tz: Tz, size: &str) -> Vec<i64> {
    let mut out = vec![from];
    let Some(start) = tz.timestamp_opt(from, 0).single() else {
        return out;
    };
    let start = start.naive_local();
    let date = start.date();
    let midnight = |date: NaiveDate| date.and_hms_opt(0, 0, 0).unwrap_or(start);
    let mut push = |at: i64| {
        let fits = at < to && out.len() <= MAX_BUCKETS;
        if fits && at > out[out.len() - 1] {
            out.push(at);
        }
        fits
    };
    // An hour bucket starts at each local full hour. The candidates step a quarter hour in
    // real time, so a repeated hour at a clock change is its own bucket, and a clock change of
    // 30 minutes keeps the local borders.
    // The first candidate can already be a later full hour (after a clock change gap). Some
    // historic offsets have no full hours in UTC steps; the candidates have a limit.
    if size == "hour" {
        let mut at = local(tz, date.and_hms_opt(start.hour(), 0, 0).unwrap_or(start));
        for _ in 0..MAX_BUCKETS * 8 {
            let full_hour = tz
                .timestamp_opt(at, 0)
                .single()
                .is_some_and(|t| t.minute() == 0 && t.second() == 0);
            if at > from && full_hour && !push(at) {
                break;
            }
            if at >= to {
                break;
            }
            at += 900;
        }
        return out;
    }
    let mut current = match size {
        "day" => midnight(date),
        "week" => midnight(date - Duration::days(i64::from(date.weekday().num_days_from_monday()))),
        _ => midnight(NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)),
    };
    loop {
        current = match size {
            "day" => current + Duration::days(1),
            "week" => current + Duration::weeks(1),
            _ => {
                let (year, month) = match current.month() {
                    12 => (current.year() + 1, 1),
                    month => (current.year(), month + 1),
                };
                NaiveDate::from_ymd_opt(year, month, 1).map_or(current + Duration::days(31), midnight)
            }
        };
        if !push(local(tz, current)) {
            return out;
        }
    }
}

fn auto_bucket(from: i64, to: i64) -> &'static str {
    const DAY: i64 = 86_400;
    match to - from {
        span if span <= 2 * DAY => "hour",
        span if span <= 92 * DAY => "day",
        span if span <= 366 * DAY => "week",
        _ => "month",
    }
}

/// The SQL sums; `aggregate` reads them in the same order.
fn push_sums(query: &mut QueryBuilder<Sqlite>) {
    query.push(
        "COUNT(DISTINCT CASE WHEN u.component = 'model' THEN u.request_id END), \
         COUNT(DISTINCT CASE WHEN u.component = 'model' AND u.status_code >= 400 THEN u.request_id END), \
         COALESCE(SUM(u.cost_nano), 0), COALESCE(SUM(u.reported_cost_nano), 0), \
         SUM(CASE WHEN u.cost_nano IS NULL AND u.usage_status <> 'none' THEN 1 ELSE 0 END), \
         SUM(CASE WHEN u.cost_nano IS NOT NULL AND u.cost_complete = 0 AND u.usage_status <> 'none' THEN 1 ELSE 0 END)",
    );
    for category in CATEGORIES {
        query.push(format!(", CAST(COALESCE(SUM(u.{category}), 0) AS REAL)"));
    }
    for category in CATEGORIES {
        query.push(format!(
            ", CAST(COALESCE(SUM(json_extract(u.cost_parts, '$.{category}')), 0) AS REAL)"
        ));
    }
}

fn aggregate(row: &SqliteRow, start: usize) -> Result<StatsTotals, sqlx::Error> {
    let mut totals = StatsTotals {
        requests: row.try_get(start)?,
        errors: row.try_get(start + 1)?,
        cost_nano: row.try_get(start + 2)?,
        reported_cost_nano: row.try_get(start + 3)?,
        unpriced: row.try_get::<Option<i64>, _>(start + 4)?.unwrap_or(0),
        incomplete: row.try_get::<Option<i64>, _>(start + 5)?.unwrap_or(0),
        ..StatsTotals::default()
    };
    let amounts = start + 6;
    let costs = amounts + CATEGORIES.len();
    for (index, category) in CATEGORIES.iter().enumerate() {
        let amount: f64 = row.try_get(amounts + index)?;
        totals.amounts.insert((*category).to_owned(), amount);
        let cost: f64 = row.try_get(costs + index)?;
        totals.costs.insert((*category).to_owned(), cost.round() as i64);
    }
    Ok(totals)
}

fn group_expression(group: &str) -> &'static str {
    match group {
        "key" => "COALESCE(CAST(u.api_key_id AS TEXT), 'deleted')",
        "model" => "COALESCE(u.resolved_model, u.requested_model)",
        "upstream" => {
            "CASE WHEN u.chatgpt_account_id IS NOT NULL THEN 'chatgpt:' || u.chatgpt_account_id \
             WHEN u.connection_id IS NOT NULL THEN 'connection:' || u.connection_id ELSE u.upstream END"
        }
        _ => "'all'",
    }
}

/// `WHERE` for the range and the filters.
fn push_filters(query: &mut QueryBuilder<Sqlite>, req: &StatsRequest) {
    query.push(" WHERE u.time >= ").push_bind(req.from);
    query.push(" AND u.time < ").push_bind(req.to);
    let list = |query: &mut QueryBuilder<Sqlite>, column: &str, values: Vec<String>| {
        if values.is_empty() {
            return;
        }
        query.push(format!(" AND {column} IN ("));
        let mut separated = query.separated(", ");
        for value in values {
            separated.push_bind(value);
        }
        query.push(")");
    };
    list(
        query,
        "CAST(u.api_key_id AS TEXT)",
        req.keys.iter().map(i64::to_string).collect(),
    );
    list(
        query,
        "COALESCE(u.resolved_model, u.requested_model)",
        req.models.clone(),
    );
    list(query, group_expression("upstream"), req.upstreams.clone());
}

async fn labels(state: &AppState, group: &str, keys: &[String]) -> Result<HashMap<String, String>, sqlx::Error> {
    let mut out = HashMap::new();
    match group {
        "key" => {
            let names: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM api_keys")
                .fetch_all(&state.db)
                .await?;
            out.extend(names.into_iter().map(|(id, name)| (id.to_string(), name)));
            out.insert("deleted".into(), "Deleted key".into());
        }
        "upstream" => {
            let accounts: Vec<(i64, Option<String>, Option<String>)> =
                sqlx::query_as("SELECT id, label, email FROM chatgpt_accounts")
                    .fetch_all(&state.db)
                    .await?;
            for (id, label, email) in accounts {
                let name = label.or(email).unwrap_or_else(|| format!("Account {id}"));
                out.insert(format!("chatgpt:{id}"), format!("ChatGPT · {name}"));
            }
            let connections: Vec<(i64, String)> = sqlx::query_as("SELECT id, display_name FROM connections")
                .fetch_all(&state.db)
                .await?;
            out.extend(
                connections
                    .into_iter()
                    .map(|(id, name)| (format!("connection:{id}"), name)),
            );
        }
        "none" => {
            out.insert("all".into(), "All requests".into());
        }
        // The image-generation tool reports its image model without a connection name.
        _ => {
            for key in keys.iter().filter(|key| !key.contains('/')) {
                out.insert(key.clone(), format!("{key} (image tool)"));
            }
        }
    }
    // Rows of deleted accounts or connections keep only their kind.
    for key in keys {
        out.entry(key.clone()).or_insert_with(|| key.clone());
    }
    Ok(out)
}

fn validate(req: &StatsRequest) -> ApiResult<(Tz, &'static str)> {
    if req.from >= req.to {
        return Err(ApiError::bad_request("The start must be before the end."));
    }
    // Unix seconds of the year 3000.
    if req.from < 0 || req.to > 32_503_680_000 {
        return Err(ApiError::bad_request("The range must be between 1970 and 3000."));
    }
    let tz = settings::time_zone(&req.time_zone).map_err(ApiError::bad_request)?;
    if !["key", "model", "upstream", "none"].contains(&req.group.as_str()) {
        return Err(ApiError::bad_request("StatsGroup by key, model, upstream or none."));
    }
    let bucket = match req.bucket.as_deref() {
        None | Some("") => auto_bucket(req.from, req.to),
        Some("hour") => "hour",
        Some("day") => "day",
        Some("week") => "week",
        Some("month") => "month",
        Some(_) => return Err(ApiError::bad_request("The bucket must be hour, day, week or month.")),
    };
    if req.keys.len() + req.models.len() + req.upstreams.len() > MAX_FILTER {
        return Err(ApiError::bad_request("Select fewer filters."));
    }
    Ok((tz, bucket))
}

/// StatsTotals, a series per group and bucket, and the cost per category. Buckets have local
/// borders in the time zone of the request.
#[utoipa::path(post, path = "/stats", tag = "stats", request_body = StatsRequest, responses(
    (status = OK, body = StatsResponse),
    (status = BAD_REQUEST, body = ErrorBody),
))]
async fn stats(
    _: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<StatsRequest>,
) -> ApiResult<Json<StatsResponse>> {
    let (tz, bucket) = validate(&req)?;
    let starts = buckets(req.from, req.to, tz, bucket);
    if starts.len() > MAX_BUCKETS {
        return Err(ApiError::bad_request(
            "The range has too many buckets. Use a larger bucket.",
        ));
    }
    let group = group_expression(&req.group);

    // StatsTotals of the whole range.
    let mut query = QueryBuilder::new("SELECT ");
    push_sums(&mut query);
    query.push(" FROM usage u");
    push_filters(&mut query, &req);
    // All reads use one snapshot, so the totals, the groups and the series agree.
    let mut tx = state.db.begin().await?;
    let row = query.build().fetch_one(&mut *tx).await?;
    let totals = aggregate(&row, 0)?;

    // StatsTotals per group.
    let mut query = QueryBuilder::new(format!("SELECT {group}, "));
    push_sums(&mut query);
    query.push(" FROM usage u");
    push_filters(&mut query, &req);
    query.push(" GROUP BY 1");
    let rows = query.build().fetch_all(&mut *tx).await?;
    let mut groups: Vec<(String, StatsTotals)> = Vec::with_capacity(rows.len());
    for row in &rows {
        groups.push((row.try_get(0)?, aggregate(row, 1)?));
    }

    // One point per bucket and group. The buckets go in as a list of intervals.
    let mut query = QueryBuilder::new("WITH b(i, s, e) AS (VALUES ");
    let mut separated = query.separated(", ");
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(req.to);
        separated.push("(");
        separated.push_bind_unseparated(index as i64);
        separated.push_unseparated(", ");
        separated.push_bind_unseparated(*start);
        separated.push_unseparated(", ");
        separated.push_bind_unseparated(end);
        separated.push_unseparated(")");
    }
    let inputs: Vec<String> = CATEGORIES[..INPUTS].iter().map(|c| format!("u.{c}")).collect();
    let outputs: Vec<String> = CATEGORIES[INPUTS..TOKENS].iter().map(|c| format!("u.{c}")).collect();
    query.push(format!(
        ") SELECT b.i, {group}, COUNT(DISTINCT CASE WHEN u.component = 'model' THEN u.request_id END), \
         COALESCE(SUM({}), 0), COALESCE(SUM({}), 0), \
         COALESCE(SUM(u.cost_nano), 0) FROM b JOIN usage u ON u.time >= b.s AND u.time < b.e",
        inputs.join(" + "),
        outputs.join(" + "),
    ));
    push_filters(&mut query, &req);
    query.push(" GROUP BY 1, 2");
    let rows = query.build().fetch_all(&mut *tx).await?;
    let mut series: HashMap<String, Vec<StatsPoint>> = HashMap::new();
    for row in &rows {
        let index: i64 = row.try_get(0)?;
        let key: String = row.try_get(1)?;
        let points = series
            .entry(key)
            .or_insert_with(|| vec![StatsPoint::default(); starts.len()]);
        if let Some(point) = points.get_mut(index as usize) {
            *point = StatsPoint {
                requests: row.try_get(2)?,
                input_tokens: row.try_get(3)?,
                output_tokens: row.try_get(4)?,
                cost_nano: row.try_get(5)?,
            };
        }
    }

    let keys: Vec<String> = groups.iter().map(|(key, _)| key.clone()).collect();
    let labels = labels(&state, &req.group, &keys).await?;
    let mut groups: Vec<StatsGroup> = groups
        .into_iter()
        .map(|(key, totals)| StatsGroup {
            label: labels.get(&key).cloned().unwrap_or_else(|| key.clone()),
            series: series
                .remove(&key)
                .unwrap_or_else(|| vec![StatsPoint::default(); starts.len()]),
            key,
            totals,
        })
        .collect();
    groups.sort_by(|a, b| {
        (b.totals.cost_nano, b.totals.requests)
            .cmp(&(a.totals.cost_nano, a.totals.requests))
            .then_with(|| a.label.cmp(&b.label))
    });

    // Refused requests have no key or model, so the filters do not apply to them.
    let first_hour = req.from - req.from.rem_euclid(3600);
    let rejected: Vec<(String, i64)> =
        sqlx::query_as("SELECT reason, SUM(count) FROM rejected_requests WHERE hour >= ? AND hour < ? GROUP BY reason")
            .bind(first_hour)
            .bind(req.to)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;

    Ok(Json(StatsResponse {
        bucket: bucket.to_owned(),
        buckets: starts,
        totals,
        rejected: rejected.into_iter().collect(),
        groups,
    }))
}
