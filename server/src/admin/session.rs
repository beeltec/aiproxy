use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use sqlx::SqlitePool;

use crate::crypto::{random_token, sha256};
use crate::db::now;
use crate::error::ApiError;
use crate::state::AppState;

pub const COOKIE_NAME: &str = "__Host-aiproxy_session";
const IDLE_TIMEOUT: i64 = 12 * 60 * 60;
const ABSOLUTE_TIMEOUT: i64 = 7 * 24 * 60 * 60;
/// A session that waits for the second factor is valid this long.
const PENDING_TIMEOUT: i64 = 5 * 60;
/// `last_seen_at` is written at most this often.
const TOUCH_INTERVAL: i64 = 60;
const MAX_USER_AGENT: usize = 256;

/// A logged-in admin with all required factors. Most handlers need this.
pub struct AdminSession {
    pub session_id: i64,
    pub admin_id: i64,
    pub username: String,
}

/// A session after a correct password that still waits for the second factor.
/// Only the second-factor check accepts it.
pub struct PendingSession {
    pub session_id: i64,
    pub admin_id: i64,
    pub username: String,
}

/// What must still be true when the session is stored. The check runs in the same SQL statement
/// as the insert, so a password reset or a factor reset at the same time cannot leave a session.
pub enum Proof<'a> {
    /// The admin still has this password hash.
    Password(&'a str),
    /// This passkey of the admin still exists.
    Passkey(i64),
    /// This pending session of the admin still exists.
    Pending(i64),
}

pub enum Kind {
    Full,
    /// Waits for the second factor.
    Pending,
}

pub struct ClientInfo<'a> {
    pub ip: &'a str,
    pub user_agent: Option<&'a str>,
}

/// The session insert with the admin check and one proof condition.
macro_rules! insert_session {
    ($proof:literal) => {
        concat!(
            "INSERT INTO sessions
                 (token_hash, admin_id, created_at, last_seen_at, expires_at, ip, user_agent, pending_second_factor)
             SELECT ?, id, ?, ?, ?, ?, ?, ? FROM admins WHERE id = ? AND disabled = 0 AND ",
            $proof
        )
    };
}

/// Creates a session and returns the cookie that carries its token.
/// Returns `None` when the admin is disabled or the proof does not hold any more.
pub async fn create(
    db: &SqlitePool,
    admin_id: i64,
    proof: Proof<'_>,
    kind: Kind,
    client: ClientInfo<'_>,
) -> Result<Option<Cookie<'static>>, sqlx::Error> {
    let token = random_token(32);
    let now = now();
    let (pending, lifetime) = match kind {
        Kind::Full => (false, ABSOLUTE_TIMEOUT),
        Kind::Pending => (true, PENDING_TIMEOUT),
    };
    let (sql, proof_value) = match proof {
        Proof::Password(hash) => (insert_session!("password_hash = ?"), ProofValue::Text(hash.to_owned())),
        Proof::Passkey(id) => (
            insert_session!("EXISTS (SELECT 1 FROM admin_passkeys p WHERE p.id = ? AND p.admin_id = admins.id)"),
            ProofValue::Id(id),
        ),
        Proof::Pending(id) => (
            insert_session!(
                "EXISTS (SELECT 1 FROM sessions s WHERE s.id = ? AND s.admin_id = admins.id
                     AND s.pending_second_factor = 1 AND s.expires_at > unixepoch())"
            ),
            ProofValue::Id(id),
        ),
    };
    let user_agent = client
        .user_agent
        .map(|ua| ua.chars().take(MAX_USER_AGENT).collect::<String>());
    delete_expired(db).await?;

    let query = sqlx::query(sql)
        .bind(sha256(token.as_bytes()))
        .bind(now)
        .bind(now)
        .bind(now + lifetime)
        .bind(client.ip)
        .bind(user_agent)
        .bind(pending)
        .bind(admin_id);
    let query = match proof_value {
        ProofValue::Text(value) => query.bind(value),
        ProofValue::Id(value) => query.bind(value),
    };
    if query.execute(db).await?.rows_affected() == 0 {
        return Ok(None);
    }
    if !pending {
        sqlx::query("UPDATE admins SET last_login_at = ? WHERE id = ?")
            .bind(now)
            .bind(admin_id)
            .execute(db)
            .await?;
    }
    Ok(Some(cookie(token, lifetime)))
}

enum ProofValue {
    Text(String),
    Id(i64),
}

async fn delete_expired(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let now = now();
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ? OR last_seen_at <= ?")
        .bind(now)
        .bind(idle_cutoff(now))
        .execute(db)
        .await?;
    Ok(())
}

/// Sessions last seen at or before this time are expired.
pub fn idle_cutoff(now: i64) -> i64 {
    now - IDLE_TIMEOUT
}

fn cookie(token: String, lifetime: i64) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, token))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .path("/")
        .max_age(time::Duration::seconds(lifetime))
        .build()
}

pub fn removal_cookie() -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, ""))
        .path("/")
        .secure(true)
        .http_only(true)
        .build()
}

/// Deletes the session of the given cookie, if it exists.
pub async fn delete_by_cookie(db: &SqlitePool, jar: &CookieJar) -> Result<(), sqlx::Error> {
    if let Some(cookie) = jar.get(COOKIE_NAME) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(sha256(cookie.value().as_bytes()))
            .execute(db)
            .await?;
    }
    Ok(())
}

pub async fn delete(db: &SqlitePool, session_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(session_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Deletes all sessions of an admin, optionally except one.
pub async fn delete_all_of(
    db: impl sqlx::SqliteExecutor<'_>,
    admin_id: i64,
    except: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE admin_id = ? AND id IS NOT ?")
        .bind(admin_id)
        .bind(except)
        .execute(db)
        .await?;
    Ok(())
}

/// Loads the session of the request cookie. `pending` selects which kind is accepted.
async fn load(parts: &Parts, state: &AppState, pending: bool) -> Result<(i64, i64, String), ApiError> {
    let jar = CookieJar::from_headers(&parts.headers);
    let token = jar.get(COOKIE_NAME).ok_or_else(ApiError::unauthorized)?;
    let now = now();
    let row: Option<(i64, i64, String, i64, i64)> = sqlx::query_as(
        "SELECT s.id, a.id, a.username, s.last_seen_at, s.expires_at
         FROM sessions s JOIN admins a ON a.id = s.admin_id
         WHERE s.token_hash = ? AND a.disabled = 0 AND s.pending_second_factor = ?",
    )
    .bind(sha256(token.value().as_bytes()))
    .bind(pending)
    .fetch_optional(&state.db)
    .await?;
    let Some((session_id, admin_id, username, last_seen_at, expires_at)) = row else {
        return Err(ApiError::unauthorized());
    };

    if now >= expires_at || now - last_seen_at >= IDLE_TIMEOUT {
        delete(&state.db, session_id).await?;
        return Err(ApiError::unauthorized());
    }
    if now - last_seen_at >= TOUCH_INTERVAL {
        sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
            .bind(now)
            .bind(session_id)
            .execute(&state.db)
            .await?;
    }
    Ok((session_id, admin_id, username))
}

impl FromRequestParts<AppState> for AdminSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let (session_id, admin_id, username) = load(parts, state, false).await?;
        Ok(Self {
            session_id,
            admin_id,
            username,
        })
    }
}

impl FromRequestParts<AppState> for PendingSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let (session_id, admin_id, username) = load(parts, state, true).await?;
        Ok(Self {
            session_id,
            admin_id,
            username,
        })
    }
}
