//! Token refresh. OpenAI replaces the refresh token on every refresh, so two refreshes with the
//! same old token would break the account. One refresh per account runs at a time; callers that
//! wait get the result of the refresh before them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::accounts::aad;
use super::oauth::{self, RefreshError};
use crate::db::now;
use crate::state::AppState;

/// A refresh younger than this counts as fresh, so waiting callers do not refresh again.
const FRESH_SECS: i64 = 30;
/// After a failed refresh, request-time callers do not start a new one for this long.
const COOLDOWN_SECS: i64 = 60;
const SAVE_ATTEMPTS: u64 = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];

#[derive(Default)]
pub struct Refresher {
    locks: Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>,
    /// Accounts with a running retry sequence, and the credential generation it is for.
    retrying: Mutex<HashMap<i64, i64>>,
}

/// Why the refresh runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The cron plan.
    Scheduled,
    /// An upstream call needs a fresh token. Short timeout, and respects the cooldown.
    Request,
    /// The "refresh now" button.
    Manual,
    /// A retry after a temporary failure.
    Retry,
}

#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("the account must be linked again: {0}")]
    NeedsRelogin(String),
    #[error("{0}")]
    Temporary(String),
}

impl Refresher {
    /// The per-account lock. Re-linking takes it too.
    pub async fn lock(&self, account: i64) -> OwnedMutexGuard<()> {
        let lock = self
            .locks
            .lock()
            .expect("refresh locks")
            .entry(account)
            .or_default()
            .clone();
        lock.lock_owned().await
    }
}

#[derive(sqlx::FromRow)]
struct RefreshRow {
    chatgpt_account_id: String,
    refresh_token_enc: Vec<u8>,
    credential_generation: i64,
    last_refresh_at: i64,
    last_refresh_failed_at: Option<i64>,
    last_refresh_error: Option<String>,
    status: String,
}

/// Refreshes the tokens of an account. The work runs in its own task: when the caller goes away
/// (for example a closed browser), the rotated token is still saved.
pub async fn refresh(state: &AppState, account: i64, trigger: Trigger) -> Result<(), Failure> {
    let state = state.clone();
    tokio::spawn(async move { refresh_now(&state, account, trigger).await })
        .await
        .unwrap_or_else(|_| Err(Failure::Temporary("the refresh task stopped".into())))
}

async fn refresh_now(state: &AppState, account: i64, trigger: Trigger) -> Result<(), Failure> {
    let requested_at = now();
    let guard = if trigger == Trigger::Request {
        tokio::time::timeout(REQUEST_TIMEOUT, state.refresher.lock(account))
            .await
            .map_err(|_| Failure::Temporary("another refresh of this account is still running".into()))?
    } else {
        state.refresher.lock(account).await
    };

    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT chatgpt_account_id, refresh_token_enc, credential_generation, last_refresh_at,
             last_refresh_failed_at, last_refresh_error, status
         FROM chatgpt_accounts WHERE id = ?",
    )
    .bind(account)
    .fetch_optional(&state.db)
    .await
    .map_err(db_failure)?;
    let Some(RefreshRow {
        chatgpt_account_id: chatgpt_id,
        refresh_token_enc: refresh_enc,
        credential_generation: generation,
        last_refresh_at,
        last_refresh_failed_at: last_failed_at,
        last_refresh_error,
        status,
    }) = row
    else {
        return Err(Failure::NeedsRelogin("the account does not exist".into()));
    };
    if status == "needs_relogin" {
        return Err(Failure::NeedsRelogin("the refresh token does not work any more".into()));
    }
    // A refresh that ended while this caller waited for the lock answers for it too.
    if last_refresh_at >= requested_at {
        return Ok(());
    }
    if last_failed_at.is_some_and(|at| at >= requested_at) {
        return Err(Failure::Temporary(
            last_refresh_error.unwrap_or_else(|| "the refresh failed".into()),
        ));
    }
    if trigger == Trigger::Scheduled
        && state.refresher.retrying.lock().expect("retrying").get(&account) == Some(&generation)
    {
        return Err(Failure::Temporary("retries after a failed refresh are running".into()));
    }
    let now = now();
    if trigger != Trigger::Manual && now - last_refresh_at < FRESH_SECS {
        return Ok(());
    }
    if trigger == Trigger::Request && last_failed_at.is_some_and(|at| now - at < COOLDOWN_SECS) {
        return Err(Failure::Temporary("the last refresh failed a moment ago".into()));
    }

    let refresh_token = state
        .secrets
        .decrypt(&aad(&chatgpt_id, "refresh_token"), &refresh_enc)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| Failure::NeedsRelogin("the stored refresh token cannot be read".into()))?;
    let timeout = if trigger == Trigger::Request {
        REQUEST_TIMEOUT
    } else {
        BACKGROUND_TIMEOUT
    };

    let result = oauth::refresh(&state.http, &refresh_token, timeout).await;
    // The end time, so callers that waited during the request see this result.
    let now = crate::db::now();
    match result {
        Ok(tokens) => {
            let enc = |field: &str, value: &str| state.secrets.encrypt(&aad(&chatgpt_id, field), value.as_bytes());
            let access = enc("access_token", &tokens.access_token);
            let refresh = tokens.refresh_token.as_deref().map(|t| enc("refresh_token", t));
            let id_token = tokens.id_token.as_deref().map(|t| enc("id_token", t));
            // Only for the credentials this refresh started with; a re-link in the meantime wins.
            // The old refresh token no longer works, so a failed save is tried again (the lock is
            // still held) instead of losing the new token.
            let mut attempt = 0;
            loop {
                let saved = sqlx::query(
                    "UPDATE chatgpt_accounts SET access_token_enc = ?, refresh_token_enc = COALESCE(?, refresh_token_enc),
                         id_token_enc = COALESCE(?, id_token_enc), access_expires_at = ?, last_refresh_at = ?,
                         last_refresh_error = NULL, last_refresh_failed_at = NULL, status = 'active'
                     WHERE id = ? AND credential_generation = ?",
                )
                .bind(&access)
                .bind(&refresh)
                .bind(&id_token)
                .bind(oauth::expires_at(&tokens.access_token))
                // The time of the write, so a caller that waited meanwhile shares this result.
                .bind(crate::db::now())
                .bind(account)
                .bind(generation)
                .execute(&state.db)
                .await;
                match saved {
                    Ok(_) => break,
                    Err(err) if attempt < SAVE_ATTEMPTS => {
                        attempt += 1;
                        tracing::warn!(account, error = %err, attempt, "cannot save refreshed tokens, trying again");
                        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                    }
                    Err(err) => return Err(db_failure(err)),
                }
            }
            tracing::info!(account, ?trigger, "ChatGPT token refreshed");
            Ok(())
        }
        Err(RefreshError::Permanent(message)) => {
            sqlx::query(
                "UPDATE chatgpt_accounts SET status = 'needs_relogin', last_refresh_error = ?, last_refresh_failed_at = ?
                 WHERE id = ? AND credential_generation = ?",
            )
            .bind(&message)
            .bind(now)
            .bind(account)
            .bind(generation)
            .execute(&state.db)
            .await
            .map_err(db_failure)?;
            tracing::warn!(account, %message, "ChatGPT account needs a new login");
            Err(Failure::NeedsRelogin(message))
        }
        Err(RefreshError::Temporary(message)) => {
            sqlx::query(
                "UPDATE chatgpt_accounts SET last_refresh_error = ?, last_refresh_failed_at = ?
                 WHERE id = ? AND credential_generation = ?",
            )
            .bind(&message)
            .bind(now)
            .bind(account)
            .bind(generation)
            .execute(&state.db)
            .await
            .map_err(db_failure)?;
            tracing::warn!(account, %message, "ChatGPT token refresh failed");
            drop(guard);
            if trigger != Trigger::Retry {
                start_retries(state, account, generation);
            }
            Err(Failure::Temporary(message))
        }
    }
}

/// Tries again after 30 s, 2 min and 10 min, once per account at a time. The sequence stops
/// when the credentials change (re-link).
fn start_retries(state: &AppState, account: i64, generation: i64) {
    {
        let mut retrying = state.refresher.retrying.lock().expect("retrying");
        if retrying.get(&account) == Some(&generation) {
            return;
        }
        retrying.insert(account, generation);
    }
    let started = now();
    let state = state.clone();
    tokio::spawn(async move {
        let mut failing = true;
        for delay in RETRY_DELAYS {
            tokio::time::sleep(delay).await;
            // Stop when the account was linked again or another refresh succeeded meanwhile.
            let (current, last_refresh_at, last_failed_at) = match progress(&state, account).await {
                Ok(Some(progress)) => progress,
                // The account was removed.
                Ok(None) => {
                    failing = false;
                    break;
                }
                Err(err) => {
                    tracing::warn!(account, error = %err, "cannot read the refresh state; retrying later");
                    continue;
                }
            };
            // Resolved: the latest attempt after this sequence started was a success.
            let resolved = last_refresh_at >= started && last_failed_at.is_none_or(|at| at < last_refresh_at);
            if current != generation || resolved {
                failing = false;
                break;
            }
            match refresh(&state, account, Trigger::Retry).await {
                Ok(()) | Err(Failure::NeedsRelogin(_)) => {
                    failing = false;
                    break;
                }
                Err(Failure::Temporary(_)) => continue,
            }
        }
        if failing {
            give_up(&state, account, generation).await;
        }
        let mut retrying = state.refresher.retrying.lock().expect("retrying");
        if retrying.get(&account) == Some(&generation) {
            retrying.remove(&account);
        }
    });
}

async fn progress(state: &AppState, account: i64) -> Result<Option<(i64, i64, Option<i64>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT credential_generation, last_refresh_at, last_refresh_failed_at FROM chatgpt_accounts WHERE id = ?",
    )
    .bind(account)
    .fetch_optional(&state.db)
    .await
}

/// After the last retry failed, the account needs a new login. Only if it still fails: a
/// successful refresh or a re-link in the meantime keeps it active.
async fn give_up(state: &AppState, account: i64, generation: i64) {
    let result = sqlx::query(
        "UPDATE chatgpt_accounts SET status = 'needs_relogin'
         WHERE id = ? AND credential_generation = ? AND status = 'active'
             AND last_refresh_failed_at > last_refresh_at",
    )
    .bind(account)
    .bind(generation)
    .execute(&state.db)
    .await;
    match result {
        Ok(done) if done.rows_affected() > 0 => {
            tracing::warn!(
                account,
                "ChatGPT token refresh failed several times; the account needs a new login"
            );
        }
        Ok(_) => {}
        Err(err) => tracing::error!(error = %err, "cannot mark the account"),
    }
}

fn db_failure(err: sqlx::Error) -> Failure {
    tracing::error!(error = %err, "database error in token refresh");
    Failure::Temporary("database error".into())
}
