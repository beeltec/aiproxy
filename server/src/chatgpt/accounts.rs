//! Stored ChatGPT accounts and their tokens.

use crate::db::now;
use crate::state::AppState;

use super::link::session_active;
use super::oauth::{self, OAuthError, TokenSet};

/// Associated data for an encrypted token field. The ChatGPT account id never changes, so the
/// value can be written before the row id exists.
pub fn aad(chatgpt_account_id: &str, field: &str) -> String {
    format!("chatgpt:{chatgpt_account_id}:{field}")
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("the account was removed during the sign-in; try again")]
    Removed,
    #[error("the sign-in was cancelled")]
    Cancelled,
}

/// Stores the tokens of a new login. The same ChatGPT account updates its row (re-link); the
/// credential generation goes up, so a refresh that started before does not overwrite them.
/// Returns the row id.
/// `session` is the admin session of the sign-in. The writes check in the same statement that it
/// still exists, so a revoked admin cannot save an account.
pub async fn store_link(state: &AppState, tokens: &TokenSet, session: i64) -> Result<i64, LinkError> {
    let identity = oauth::identity(&tokens.id_token)?;
    let id = &identity.account_id;
    let enc = |field: &str, value: &str| state.secrets.encrypt(&aad(id, field), value.as_bytes());

    // Two attempts: when a parallel first login inserted the account just now, update it.
    for _ in 0..2 {
        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM chatgpt_accounts WHERE chatgpt_account_id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
        if let Some(row_id) = existing {
            let _lock = state.refresher.lock(row_id).await;
            let updated = sqlx::query(concat!(
                "UPDATE chatgpt_accounts SET email = ?, plan_type = ?, access_token_enc = ?, refresh_token_enc = ?,
                     id_token_enc = ?, access_expires_at = ?, credential_generation = credential_generation + 1,
                     last_refresh_at = ?, last_refresh_error = NULL, last_refresh_failed_at = NULL, status = 'active'
                 WHERE id = ? AND ",
                session_active!()
            ))
            .bind(&identity.email)
            .bind(&identity.plan_type)
            .bind(enc("access_token", &tokens.access_token))
            .bind(enc("refresh_token", &tokens.refresh_token))
            .bind(enc("id_token", &tokens.id_token))
            .bind(oauth::expires_at(&tokens.access_token))
            .bind(now())
            .bind(row_id)
            .bind(session)
            .execute(&state.db)
            .await?
            .rows_affected();
            if updated == 0 {
                return Err(LinkError::Removed);
            }
            tracing::info!(account = row_id, "ChatGPT account linked again");
            return Ok(row_id);
        }

        // The first account becomes the primary account.
        let inserted: Result<Option<i64>, sqlx::Error> = sqlx::query_scalar(concat!(
            "INSERT INTO chatgpt_accounts (chatgpt_account_id, email, plan_type, access_token_enc, refresh_token_enc,
                 id_token_enc, access_expires_at, last_refresh_at, is_primary, failover_order, created_at)
             SELECT ?, ?, ?, ?, ?, ?, ?, ?,
                 NOT EXISTS (SELECT 1 FROM chatgpt_accounts),
                 (SELECT COALESCE(MAX(failover_order), 0) + 1 FROM chatgpt_accounts), ?
             WHERE ",
            session_active!(),
            " RETURNING id"
        ))
        .bind(id)
        .bind(&identity.email)
        .bind(&identity.plan_type)
        .bind(enc("access_token", &tokens.access_token))
        .bind(enc("refresh_token", &tokens.refresh_token))
        .bind(enc("id_token", &tokens.id_token))
        .bind(oauth::expires_at(&tokens.access_token))
        .bind(now())
        .bind(now())
        .bind(session)
        .fetch_optional(&state.db)
        .await;
        match inserted {
            Ok(None) => return Err(LinkError::Cancelled),
            Ok(Some(row_id)) => {
                tracing::info!(account = row_id, "ChatGPT account linked");
                return Ok(row_id);
            }
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(LinkError::Removed)
}

/// What an upstream call needs.
pub struct Credentials {
    pub chatgpt_account_id: String,
    pub access_token: String,
}

/// Reads and decrypts the current access token.
pub async fn credentials(state: &AppState, account: i64) -> anyhow::Result<Credentials> {
    let (chatgpt_account_id, access_token_enc): (String, Vec<u8>) =
        sqlx::query_as("SELECT chatgpt_account_id, access_token_enc FROM chatgpt_accounts WHERE id = ?")
            .bind(account)
            .fetch_one(&state.db)
            .await?;
    let access_token = state
        .secrets
        .decrypt(&aad(&chatgpt_account_id, "access_token"), &access_token_enc)?;
    Ok(Credentials {
        chatgpt_account_id,
        access_token: String::from_utf8(access_token)?,
    })
}
