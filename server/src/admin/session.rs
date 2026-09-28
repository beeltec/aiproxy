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
/// `last_seen_at` is written at most this often.
const TOUCH_INTERVAL: i64 = 60;
const MAX_USER_AGENT: usize = 256;

/// A logged-in admin. Handlers that take this extractor need a valid session.
pub struct AdminSession {
    pub session_id: i64,
    pub admin_id: i64,
    pub username: String,
}

/// Creates a session and returns the cookie that carries its token.
pub async fn create(
    db: &SqlitePool,
    admin_id: i64,
    ip: &str,
    user_agent: Option<&str>,
) -> Result<Cookie<'static>, sqlx::Error> {
    let token = random_token(32);
    let now = now();
    let user_agent = user_agent.map(|ua| ua.chars().take(MAX_USER_AGENT).collect::<String>());
    sqlx::query(
        "INSERT INTO sessions (token_hash, admin_id, created_at, last_seen_at, expires_at, ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sha256(token.as_bytes()))
    .bind(admin_id)
    .bind(now)
    .bind(now)
    .bind(now + ABSOLUTE_TIMEOUT)
    .bind(ip)
    .bind(user_agent)
    .execute(db)
    .await?;
    sqlx::query("UPDATE admins SET last_login_at = ? WHERE id = ?")
        .bind(now)
        .bind(admin_id)
        .execute(db)
        .await?;
    Ok(cookie(token))
}

fn cookie(token: String) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, token))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .path("/")
        .max_age(time::Duration::seconds(ABSOLUTE_TIMEOUT))
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

/// Deletes all sessions of an admin, optionally except one.
pub async fn delete_all_of(db: &SqlitePool, admin_id: i64, except: Option<i64>) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE admin_id = ? AND id IS NOT ?")
        .bind(admin_id)
        .bind(except)
        .execute(db)
        .await?;
    Ok(())
}

impl FromRequestParts<AppState> for AdminSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar.get(COOKIE_NAME).ok_or_else(ApiError::unauthorized)?;
        let now = now();
        let row: Option<(i64, i64, String, i64, i64)> = sqlx::query_as(
            "SELECT s.id, a.id, a.username, s.last_seen_at, s.expires_at
             FROM sessions s JOIN admins a ON a.id = s.admin_id
             WHERE s.token_hash = ? AND a.disabled = 0",
        )
        .bind(sha256(token.value().as_bytes()))
        .fetch_optional(&state.db)
        .await?;
        let Some((session_id, admin_id, username, last_seen_at, expires_at)) = row else {
            return Err(ApiError::unauthorized());
        };

        if now >= expires_at || now - last_seen_at >= IDLE_TIMEOUT {
            sqlx::query("DELETE FROM sessions WHERE id = ?")
                .bind(session_id)
                .execute(&state.db)
                .await?;
            return Err(ApiError::unauthorized());
        }
        if now - last_seen_at >= TOUCH_INTERVAL {
            sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
                .bind(now)
                .bind(session_id)
                .execute(&state.db)
                .await?;
        }

        Ok(Self {
            session_id,
            admin_id,
            username,
        })
    }
}
