//! Scheduled token refresh.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::refresh::{self, Trigger};
use crate::settings::{self, Settings};
use crate::state::AppState;

/// Sleep at most this long, so clock changes cannot delay a run for long.
const MAX_SLEEP: Duration = Duration::from_secs(3600);

/// The refresh plan of one account.
#[derive(sqlx::FromRow)]
pub struct AccountPlan {
    pub id: i64,
    pub refresh_mode: String,
    pub refresh_cron: Option<String>,
    pub status: String,
}

/// The cron plan that applies to an account, or `None` when it has no scheduled refresh.
pub fn effective_cron(settings: &Settings, plan: &AccountPlan) -> Option<String> {
    if plan.status == "needs_relogin" {
        return None;
    }
    match plan.refresh_mode.as_str() {
        "custom" => plan.refresh_cron.clone(),
        "disabled" => None,
        _ => settings.refresh.enabled.then(|| settings.refresh.cron.clone()),
    }
}

/// The next scheduled refresh of an account.
pub fn next_refresh(settings: &Settings, plan: &AccountPlan, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let cron = settings::parse_cron(&effective_cron(settings, plan)?).ok()?;
    settings::next_runs(&cron, settings.tz(), after, 1).into_iter().next()
}

async fn plans(state: &AppState) -> Result<Vec<AccountPlan>, sqlx::Error> {
    sqlx::query_as("SELECT id, refresh_mode, refresh_cron, status FROM chatgpt_accounts")
        .fetch_all(&state.db)
        .await
}

/// Runs until the process stops. `state.schedule_changed` wakes it after changes to the settings
/// or the accounts.
pub async fn run(state: AppState) {
    loop {
        let now = Utc::now();
        let (settings, plans) = match (settings::load(&state.db).await, plans(&state).await) {
            (Ok(settings), Ok(plans)) => (settings, plans),
            (Err(err), _) | (_, Err(err)) => {
                tracing::error!(error = %err, "cannot read the refresh plans");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        let due: Vec<(i64, DateTime<Utc>)> = plans
            .iter()
            .filter_map(|plan| next_refresh(&settings, plan, now).map(|at| (plan.id, at)))
            .collect();
        let earliest = due.iter().map(|(_, at)| *at).min();
        let sleep = earliest
            .and_then(|at| (at - now).to_std().ok())
            .unwrap_or(MAX_SLEEP)
            .min(MAX_SLEEP);

        tokio::select! {
            () = tokio::time::sleep(sleep) => {}
            () = state.schedule_changed.notified() => continue,
        }

        let Some(earliest) = earliest else { continue };
        if Utc::now() < earliest {
            continue;
        }
        for (account, at) in due {
            if at <= earliest {
                let state = state.clone();
                tokio::spawn(async move {
                    // Errors are stored on the account and shown in the dashboard.
                    let _ = refresh::refresh(&state, account, Trigger::Scheduled).await;
                });
            }
        }
        // Do not run the same minute twice.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
