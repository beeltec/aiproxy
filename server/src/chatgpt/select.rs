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
/// the failover threshold. When all are skipped, the one whose limit resets first is tried, so
/// the client gets the real error.
pub async fn accounts_for(state: &AppState, model_id: i64) -> Result<Selection, sqlx::Error> {
    let settings = settings::load(&state.db).await?;
    let rows: Vec<Candidate> = sqlx::query_as(
        "SELECT a.id, a.is_primary, a.failover_enabled, a.status, a.limited_until,
             q.primary_used_percent, q.primary_reset_at, q.secondary_used_percent, q.secondary_reset_at
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
    let over =
        |used: Option<f64>, reset: Option<i64>| used.is_some_and(|u| u >= threshold) && reset.is_none_or(|r| r > now);
    let blocked_until = |c: &Candidate| -> Option<i64> {
        let mut until = c.limited_until.filter(|u| *u > now);
        // The proactive threshold applies only with failover; without it the primary is used
        // until the upstream refuses.
        if settings.failover.enabled {
            if over(c.primary_used_percent, c.primary_reset_at) {
                until = until.max(c.primary_reset_at.or(Some(now + 3600)));
            }
            if over(c.secondary_used_percent, c.secondary_reset_at) {
                until = until.max(c.secondary_reset_at.or(Some(now + 3600)));
            }
        }
        until
    };

    let usable: Vec<i64> = in_play
        .iter()
        .filter(|c| c.status == "active" && blocked_until(c).is_none())
        .map(|c| c.id)
        .collect();
    if !usable.is_empty() {
        return Ok(Selection::Accounts(usable));
    }
    let soonest = in_play
        .iter()
        .filter(|c| c.status == "active")
        .filter_map(|c| blocked_until(c).map(|until| (until, c.id)))
        .min();
    Ok(match soonest {
        Some((_, id)) => Selection::Accounts(vec![id]),
        None if in_play.is_empty() => {
            Selection::Unavailable("Only a backup account can use this model, and failover is off.".into())
        }
        None => Selection::Unavailable("The ChatGPT accounts for this model need a new login.".into()),
    })
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
    let reset = |window: &str| {
        number(&format!("x-codex-{window}-reset-at"))
            .map(|at| at as i64)
            .or_else(|| number(&format!("x-codex-{window}-reset-after-seconds")).map(|s| now + s as i64))
    };
    let primary = number("x-codex-primary-used-percent");
    let secondary = number("x-codex-secondary-used-percent");
    if primary.is_none() && secondary.is_none() {
        return;
    }
    let result = sqlx::query(
        "INSERT INTO chatgpt_quota (account_id, primary_used_percent, primary_window_minutes, primary_reset_at,
             secondary_used_percent, secondary_window_minutes, secondary_reset_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (account_id) DO UPDATE SET primary_used_percent = excluded.primary_used_percent,
             primary_window_minutes = excluded.primary_window_minutes, primary_reset_at = excluded.primary_reset_at,
             secondary_used_percent = excluded.secondary_used_percent,
             secondary_window_minutes = excluded.secondary_window_minutes,
             secondary_reset_at = excluded.secondary_reset_at, updated_at = excluded.updated_at",
    )
    .bind(account)
    .bind(primary)
    .bind(number("x-codex-primary-window-minutes").map(|m| m as i64))
    .bind(reset("primary"))
    .bind(secondary)
    .bind(number("x-codex-secondary-window-minutes").map(|m| m as i64))
    .bind(reset("secondary"))
    .bind(now)
    .execute(&state.db)
    .await;
    if let Err(err) = result {
        tracing::warn!(account, error = %err, "cannot store the usage limits");
    }
}

/// Blocks the account after a usage-limit error until `until`.
pub async fn mark_limited(state: &AppState, account: i64, until: i64) {
    let result = sqlx::query("UPDATE chatgpt_accounts SET limited_until = ? WHERE id = ?")
        .bind(until)
        .bind(account)
        .execute(&state.db)
        .await;
    if let Err(err) = result {
        tracing::warn!(account, error = %err, "cannot store the usage-limit block");
    }
}
