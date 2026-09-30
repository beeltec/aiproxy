//! The regular poll of the ChatGPT account usage. The results also set or remove the
//! usage-limit block.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use super::select::{self, Quota, Window};
use super::{accounts, backend, codex_version};
use crate::db::now;
use crate::settings;
use crate::state::AppState;

const URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const TIMEOUT: Duration = Duration::from_secs(15);
/// The loop checks the accounts at this interval. It is also the shortest poll interval.
const TICK: Duration = Duration::from_secs(30);
/// The pause after a 429 answer without a usable `Retry-After`.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const MAX_COOLDOWN: Duration = Duration::from_secs(3600);

/// The poll state of each account. It is not stored: after a restart, a cooldown is lost.
#[derive(Default)]
pub struct UsagePolls {
    accounts: Mutex<HashMap<i64, Arc<AsyncMutex<PollTimes>>>>,
}

#[derive(Default)]
struct PollTimes {
    last_attempt: Option<Instant>,
    /// Set from the `Retry-After` of a 429 answer.
    not_before: Option<Instant>,
}

impl PollTimes {
    fn throttled(&self, now: Instant) -> bool {
        self.not_before.is_some_and(|at| now < at)
    }
}

impl UsagePolls {
    fn entry(&self, account: i64) -> Arc<AsyncMutex<PollTimes>> {
        self.accounts
            .lock()
            .expect("usage polls")
            .entry(account)
            .or_default()
            .clone()
    }

    /// Removes the entries of deleted accounts. An entry in use stays.
    fn retain(&self, accounts: &HashSet<i64>) {
        self.accounts
            .lock()
            .expect("usage polls")
            .retain(|id, entry| accounts.contains(id) || Arc::strong_count(entry) > 1);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("the ChatGPT backend limits the requests now")]
    Throttled,
    /// A fixed text: upstream answers can hold secrets.
    #[error("{0}")]
    Failed(String),
}

#[derive(Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
    credits: Option<Credits>,
    rate_limit_reached_type: Option<Value>,
}

#[derive(Deserialize)]
struct RateLimit {
    allowed: Option<bool>,
    limit_reached: Option<bool>,
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_after_seconds: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct Credits {
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    balance: Option<String>,
}

impl UsageWindow {
    /// The reset time, only when it is in the future.
    fn reset(&self, now: i64) -> Option<i64> {
        self.reset_at.filter(|at| *at > now).or_else(|| {
            self.reset_after_seconds
                .filter(|seconds| *seconds > 0)
                .map(|seconds| now + seconds)
        })
    }

    fn full(&self) -> bool {
        self.used_percent.is_some_and(|used| used >= 100.0)
    }
}

impl RateLimit {
    fn windows(&self) -> impl Iterator<Item = &UsageWindow> {
        self.primary_window.iter().chain(&self.secondary_window)
    }

    /// An absent window is stored as empty, so its old values cannot block the account.
    fn quota(&self, now: i64) -> Quota {
        let window = |window: &Option<UsageWindow>| {
            window.as_ref().map_or_else(Window::default, |w| Window {
                used_percent: w.used_percent,
                // The backend gives seconds, the headers give minutes.
                window_minutes: w.limit_window_seconds.map(|seconds| seconds / 60),
                reset_at: w.reset(now),
            })
        };
        Quota {
            primary: window(&self.primary_window),
            secondary: window(&self.secondary_window),
        }
    }
}

enum Block {
    Until(i64),
    Clear,
    /// The answer does not tell: the block does not change.
    Unknown,
}

fn block(usage: &UsageResponse, limits: &RateLimit, now: i64) -> Block {
    let full = limits.windows().any(UsageWindow::full);
    let reached = limits.allowed == Some(false)
        || limits.limit_reached == Some(true)
        || usage.rate_limit_reached_type.is_some()
        || full;
    if reached {
        let until = limits
            .windows()
            .filter(|w| w.full())
            .filter_map(|w| w.reset(now))
            .max()
            .or_else(|| limits.windows().filter_map(|w| w.reset(now)).min())
            .unwrap_or(now + 3600);
        return Block::Until(until);
    }
    if limits.allowed == Some(true) && limits.limit_reached == Some(false) {
        return Block::Clear;
    }
    Block::Unknown
}

/// The poll interval of an account: shorter near the usage limit, but not for a blocked account.
fn interval(base: Duration, used: Option<f64>, blocked: bool) -> Duration {
    let Some(used) = used.filter(|_| !blocked) else {
        return base;
    };
    let mut interval = base;
    if used >= 75.0 {
        interval = interval.min(base / 2);
    }
    if used >= 90.0 {
        interval = interval.min(Duration::from_secs(60));
    }
    if used >= 99.0 {
        interval = interval.min(TICK);
    }
    interval
}

#[derive(sqlx::FromRow)]
struct PollRow {
    id: i64,
    status: String,
    access_expires_at: Option<i64>,
    limited_until: Option<i64>,
    primary_used_percent: Option<f64>,
    secondary_used_percent: Option<f64>,
}

/// Runs until the process stops. `state.usage_poll_changed` wakes it after a settings change.
pub async fn run(state: AppState) {
    loop {
        if let Err(err) = tick(&state).await {
            tracing::error!(error = %err, "cannot read the usage poll plan");
        }
        tokio::select! {
            () = state.stopping.cancelled() => return,
            () = tokio::time::sleep(TICK) => {}
            () = state.usage_poll_changed.notified() => {}
        }
    }
}

/// Polls all due accounts, concurrently.
async fn tick(state: &AppState) -> Result<(), sqlx::Error> {
    let settings = settings::load(&state.db).await?;
    if !settings.usage_poll.enabled {
        return Ok(());
    }
    let rows: Vec<PollRow> = sqlx::query_as(
        "SELECT a.id, a.status, a.access_expires_at, a.limited_until,
             q.primary_used_percent, q.secondary_used_percent
         FROM chatgpt_accounts a LEFT JOIN chatgpt_quota q ON q.account_id = a.id",
    )
    .fetch_all(&state.db)
    .await?;
    state.usage_polls.retain(&rows.iter().map(|row| row.id).collect());

    let now = now();
    let base = Duration::from_secs(u64::from(settings.usage_poll.minutes) * 60);
    let due = rows.iter().filter_map(|row| {
        // The poll does not renew tokens. The refresh plan does that.
        let expired = row.access_expires_at.is_some_and(|at| at <= now);
        if row.status != "active" || expired {
            return None;
        }
        let used = [row.primary_used_percent, row.secondary_used_percent]
            .into_iter()
            .flatten()
            .reduce(f64::max);
        let interval = interval(base, used, row.limited_until.is_some_and(|until| until > now));
        Some(poll_if_due(state, row.id, interval))
    });
    futures_util::future::join_all(due).await;
    Ok(())
}

/// Skips the account when a poll for it runs now, or when its last attempt is too recent.
async fn poll_if_due(state: &AppState, account: i64, interval: Duration) {
    let Ok(mut times) = state.usage_polls.entry(account).try_lock_owned() else {
        return;
    };
    let now = Instant::now();
    if times.throttled(now) || times.last_attempt.is_some_and(|at| now < at + interval) {
        return;
    }
    // The poll logs its errors.
    let _ = poll(state, account, &mut times).await;
}

/// Polls one account now and stores the result. Waits for a poll of the account that runs now.
pub async fn refresh(state: &AppState, account: i64) -> Result<(), UsageError> {
    let mut times = state.usage_polls.entry(account).lock_owned().await;
    if times.throttled(Instant::now()) {
        return Err(UsageError::Throttled);
    }
    poll(state, account, &mut times).await
}

/// The compare values for the checks before the write: credential generation, block revision
/// and quota revision (`None` without a quota row).
type Revisions = (i64, i64, Option<i64>);

async fn revisions(db: impl sqlx::SqliteExecutor<'_>, account: i64) -> Result<Option<Revisions>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.credential_generation, a.limited_revision, q.revision
         FROM chatgpt_accounts a LEFT JOIN chatgpt_quota q ON q.account_id = a.id WHERE a.id = ?",
    )
    .bind(account)
    .fetch_optional(db)
    .await
}

fn db_error(err: sqlx::Error) -> UsageError {
    tracing::error!(error = %err, "database error in the usage poll");
    UsageError::Failed("database error".into())
}

async fn poll(state: &AppState, account: i64, times: &mut PollTimes) -> Result<(), UsageError> {
    times.last_attempt = Some(Instant::now());
    let Some(before) = revisions(&state.db, account).await.map_err(db_error)? else {
        return Ok(());
    };
    let credentials = accounts::credentials(state, account).await.map_err(|err| {
        tracing::warn!(account, error = %err, "cannot read the account tokens");
        UsageError::Failed("cannot read the account tokens".into())
    })?;
    let installation_id = backend::installation_id(&state.db).await.map_err(db_error)?;
    let client_version = codex_version::current(state);
    let request = state
        .http
        .get(URL)
        .header("Accept", "application/json")
        .timeout(TIMEOUT);
    let response = backend::with_headers(request, &credentials, &installation_id, &client_version)
        .send()
        .await
        .map_err(|err| {
            tracing::warn!(account, error = %err, "cannot reach the ChatGPT backend");
            UsageError::Failed("cannot reach the ChatGPT backend".into())
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(account, status = status.as_u16(), "the usage request failed");
        if status.as_u16() == 429 {
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok());
            let cooldown = retry_after.and_then(retry_seconds).map_or(DEFAULT_COOLDOWN, |seconds| {
                Duration::from_secs(seconds).min(MAX_COOLDOWN)
            });
            times.not_before = Some(Instant::now() + cooldown);
            return Err(UsageError::Throttled);
        }
        return Err(UsageError::Failed(format!(
            "the ChatGPT backend answered with HTTP {}",
            status.as_u16()
        )));
    }
    let usage: UsageResponse = response.json().await.map_err(|_| {
        tracing::warn!(account, "cannot read the usage answer");
        UsageError::Failed("the ChatGPT backend gave an unexpected answer".into())
    })?;
    save(state, account, before, &usage).await.map_err(db_error)?;
    tracing::info!(account, "ChatGPT usage loaded");
    Ok(())
}

/// The seconds to wait from a `Retry-After` value: a number of seconds or an HTTP date.
/// A date that is not in the future gives `None`.
fn retry_seconds(value: &str) -> Option<u64> {
    value.parse().ok().or_else(|| {
        let at = chrono::DateTime::parse_from_rfc2822(value).ok()?.timestamp();
        u64::try_from(at - now()).ok().filter(|seconds| *seconds > 0)
    })
}

/// Data that changed during the poll is newer than the poll result: the matching part is not
/// written.
async fn save(state: &AppState, account: i64, before: Revisions, usage: &UsageResponse) -> Result<(), sqlx::Error> {
    // The write lock comes first, so no other write can come between the checks and the writes.
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    let Some((generation, limited_revision, quota_revision)) = revisions(&mut *tx, account).await? else {
        return Ok(());
    };
    // The account was linked again during the poll.
    if generation != before.0 {
        return Ok(());
    }
    let now = now();
    let limits = usage.rate_limit.as_ref().filter(|_| quota_revision == before.2);
    if let Some(limits) = limits {
        select::write_quota(&mut *tx, account, &limits.quota(now), now).await?;
        if limited_revision == before.1 {
            match block(usage, limits, now) {
                Block::Until(until) => select::write_block(&mut *tx, account, Some(until)).await?,
                Block::Clear => select::write_block(&mut *tx, account, None).await?,
                Block::Unknown => {}
            }
        }
    }
    if let Some(credits) = &usage.credits {
        sqlx::query(
            "UPDATE chatgpt_accounts SET has_credits = ?, credits_unlimited = ?, credits_balance = ? WHERE id = ?",
        )
        .bind(credits.has_credits)
        .bind(credits.unlimited)
        .bind(&credits.balance)
        .bind(account)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(plan_type) = usage
        .plan_type
        .as_deref()
        .filter(|plan| !plan.is_empty() && *plan != "unknown")
    {
        sqlx::query("UPDATE chatgpt_accounts SET plan_type = ? WHERE id = ?")
            .bind(plan_type)
            .bind(account)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
