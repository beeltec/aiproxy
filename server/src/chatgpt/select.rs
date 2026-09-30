//! Which ChatGPT account serves a request.

use crate::db::now;
use crate::settings;
use crate::state::AppState;

#[derive(sqlx::FromRow)]
struct Candidate {
    id: i64,
    is_primary: bool,
    failover_enabled: bool,
    status: String,
    limited_until: Option<i64>,
    primary_used_percent: Option<f64>,
    primary_reset_at: Option<i64>,
    secondary_used_percent: Option<f64>,
    secondary_reset_at: Option<i64>,
    quota_updated_at: Option<i64>,
}

pub enum Selection {
    /// Accounts to try, in this order.
    Accounts(Vec<i64>),
    /// No account can use this model.
    NoAccount,
    /// Accounts exist, but none can take a request now.
    Unavailable(String),
}

/// The accounts for a model: the primary first, then (with failover) the checked accounts in
/// their order. Skips accounts that need a new login, are blocked by a usage limit, or are over
/// the failover threshold. The blocked account whose limit resets first comes last: it is tried
/// when all others fail, so the client gets the real error.
pub async fn accounts_for(state: &AppState, model_id: i64) -> Result<Selection, sqlx::Error> {
    let settings = settings::load(&state.db).await?;
    let rows: Vec<Candidate> = sqlx::query_as(
        "SELECT a.id, a.is_primary, a.failover_enabled, a.status, a.limited_until,
             q.primary_used_percent, q.primary_reset_at, q.secondary_used_percent, q.secondary_reset_at,
             q.updated_at AS quota_updated_at
         FROM chatgpt_accounts a
         JOIN chatgpt_account_models m ON m.account_id = a.id AND m.model_id = ?
         LEFT JOIN chatgpt_quota q ON q.account_id = a.id
         ORDER BY a.is_primary DESC, a.failover_order, a.id",
    )
    .bind(model_id)
    .fetch_all(&state.db)
    .await?;
    if rows.is_empty() {
        return Ok(Selection::NoAccount);
    }

    let now = now();
    let threshold = f64::from(settings.failover.threshold_percent);
    let in_play: Vec<&Candidate> = rows
        .iter()
        .filter(|c| c.is_primary || (settings.failover.enabled && c.failover_enabled))
        .collect();
    let blocked_until = |c: &Candidate| -> Option<i64> {
        let mut until = c.limited_until.filter(|u| *u > now);
        // The proactive threshold applies only with failover; without it the primary is used
        // until the upstream refuses. A reading without a reset time is kept for one hour.
        if settings.failover.enabled {
            let stale_at = c.quota_updated_at.map(|at| at + 3600);
            for (used, reset) in [
                (c.primary_used_percent, c.primary_reset_at),
                (c.secondary_used_percent, c.secondary_reset_at),
            ] {
                let end = reset.or(stale_at);
                if used.is_some_and(|u| u >= threshold) && end.is_some_and(|e| e > now) {
                    until = until.max(end);
                }
            }
        }
        until
    };

    let mut usable: Vec<i64> = in_play
        .iter()
        .filter(|c| c.status == "active" && blocked_until(c).is_none())
        .map(|c| c.id)
        .collect();
    let soonest = in_play
        .iter()
        .filter(|c| c.status == "active")
        .filter_map(|c| blocked_until(c).map(|until| (until, c.id)))
        .min();
    // The blocked account that resets first comes last, for when all others fail.
    usable.extend(soonest.map(|(_, id)| id));
    if !usable.is_empty() {
        return Ok(Selection::Accounts(usable));
    }
    Ok(if in_play.is_empty() {
        Selection::Unavailable("Only a backup account can use this model, and failover is off.".into())
    } else {
        Selection::Unavailable("The ChatGPT accounts for this model need a new login.".into())
    })
}

/// The usage-limit windows of an account (usually a 5-hour and a weekly window).
#[derive(Default)]
pub struct Quota {
    pub primary: Window,
    pub secondary: Window,
}

#[derive(Default)]
pub struct Window {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<i64>,
    pub reset_at: Option<i64>,
}

/// Stores the usage-limit headers of a backend answer.
pub async fn store_quota(state: &AppState, account: i64, headers: &reqwest::header::HeaderMap) {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
    };
    let now = now();
    // Two header forms are in use: an absolute reset time, or seconds until the reset.
    let window = |window: &str| Window {
        used_percent: number(&format!("x-codex-{window}-used-percent")),
        window_minutes: number(&format!("x-codex-{window}-window-minutes")).map(|m| m as i64),
        reset_at: number(&format!("x-codex-{window}-reset-at"))
            .map(|at| at as i64)
            .or_else(|| number(&format!("x-codex-{window}-reset-after-seconds")).map(|s| now + s as i64)),
    };
    let quota = Quota {
        primary: window("primary"),
        secondary: window("secondary"),
    };
    if quota.primary.used_percent.is_none() && quota.secondary.used_percent.is_none() {
        return;
    }
    if let Err(err) = write_quota(&state.db, account, &quota, now).await {
        tracing::warn!(account, error = %err, "cannot store the usage limits");
    }
}

/// Replaces the stored windows of the account and increases the quota revision.
pub async fn write_quota(
    db: impl sqlx::SqliteExecutor<'_>,
    account: i64,
    quota: &Quota,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO chatgpt_quota (account_id, primary_used_percent, primary_window_minutes, primary_reset_at,
             secondary_used_percent, secondary_window_minutes, secondary_reset_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (account_id) DO UPDATE SET primary_used_percent = excluded.primary_used_percent,
             primary_window_minutes = excluded.primary_window_minutes, primary_reset_at = excluded.primary_reset_at,
             secondary_used_percent = excluded.secondary_used_percent,
             secondary_window_minutes = excluded.secondary_window_minutes,
             secondary_reset_at = excluded.secondary_reset_at, updated_at = excluded.updated_at,
             revision = chatgpt_quota.revision + 1",
    )
    .bind(account)
    .bind(quota.primary.used_percent)
    .bind(quota.primary.window_minutes)
    .bind(quota.primary.reset_at)
    .bind(quota.secondary.used_percent)
    .bind(quota.secondary.window_minutes)
    .bind(quota.secondary.reset_at)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

/// Used percent and reset time of the primary and the secondary window.
type QuotaWindows = (Option<f64>, Option<i64>, Option<f64>, Option<i64>);

/// Blocks the account after a usage-limit error. Without `until`, the block lasts until the
/// reset of a full quota window, else one hour. Returns the end of the block.
pub async fn mark_limited(state: &AppState, account: i64, until: Option<i64>) -> i64 {
    let now = now();
    let until = match until {
        Some(until) => until,
        None => {
            let windows: Option<QuotaWindows> = sqlx::query_as(
                "SELECT primary_used_percent, primary_reset_at, secondary_used_percent, secondary_reset_at
                 FROM chatgpt_quota WHERE account_id = ?",
            )
            .bind(account)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
            windows
                .and_then(|(p_used, p_reset, s_used, s_reset)| {
                    [(p_used, p_reset), (s_used, s_reset)]
                        .into_iter()
                        .filter(|(used, reset)| used.is_some_and(|u| u >= 100.0) && reset.is_some_and(|r| r > now))
                        .filter_map(|(_, reset)| reset)
                        .max()
                })
                .unwrap_or(now + 3600)
        }
    };
    if let Err(err) = write_block(&state.db, account, Some(until)).await {
        tracing::warn!(account, error = %err, "cannot store the usage-limit block");
    }
    until
}

/// Sets the end of the usage-limit block (`None` removes the block) and increases the block
/// revision.
pub async fn write_block(
    db: impl sqlx::SqliteExecutor<'_>,
    account: i64,
    until: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE chatgpt_accounts SET limited_until = ?, limited_revision = limited_revision + 1 WHERE id = ?")
        .bind(until)
        .bind(account)
        .execute(db)
        .await?;
    Ok(())
}
