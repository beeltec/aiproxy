use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::admins::{validate_password, validate_username};
use super::session::{self, AdminSession};
use crate::client_ip::ClientIp;
use crate::crypto::{random_bytes, sha256};
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;

const SETUP_TOKEN_LIFETIME: i64 = 60 * 60;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(setup_status, setup))
        .routes(routes!(login))
        .routes(routes!(logout))
        .routes(routes!(me))
}

#[derive(Serialize, ToSchema)]
pub struct SetupStatus {
    /// True when no admin exists yet.
    required: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct SetupRequest {
    token: String,
    username: String,
    password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize, ToSchema)]
pub struct Me {
    id: i64,
    username: String,
}

/// Creates the setup token for the first admin. Returns the token.
pub async fn create_setup_token(db: &SqlitePool) -> anyhow::Result<String> {
    if admin_count(db).await? > 0 {
        anyhow::bail!("an admin exists already; setup is closed");
    }
    // Hex, so the token never starts with "-" when it is pasted into a shell.
    let token: String = random_bytes(20).iter().map(|b| format!("{b:02x}")).collect();
    let now = now();
    sqlx::query(
        "INSERT INTO setup_token (id, token_hash, created_at, expires_at) VALUES (1, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET token_hash = excluded.token_hash,
             created_at = excluded.created_at, expires_at = excluded.expires_at",
    )
    .bind(sha256(token.as_bytes()))
    .bind(now)
    .bind(now + SETUP_TOKEN_LIFETIME)
    .execute(db)
    .await?;
    Ok(token)
}

async fn admin_count(db: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM admins").fetch_one(db).await
}

#[utoipa::path(get, path = "/setup", tag = "auth", responses((status = OK, body = SetupStatus)))]
async fn setup_status(State(state): State<AppState>) -> ApiResult<Json<SetupStatus>> {
    Ok(Json(SetupStatus {
        required: admin_count(&state.db).await? == 0,
    }))
}

/// Creates the first admin with the setup token and logs in.
#[utoipa::path(post, path = "/setup", tag = "auth", request_body = SetupRequest, responses(
    (status = OK, body = Me),
    (status = FORBIDDEN, body = ErrorBody, description = "Setup is closed or the token is wrong"),
))]
async fn setup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<SetupRequest>,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let ip = ip.to_string();
    if !state.login.attempts.try_record(&ip) {
        return Err(ApiError::too_many_requests("Too many attempts. Wait a minute."));
    }
    validate_username(&req.username)?;
    validate_password(&req.password)?;
    let forbidden = || {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "setup_closed",
            "Setup is closed or the token is not valid.",
        )
    };
    // Checked before the expensive hash; the transaction below checks again.
    let token_valid: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM setup_token WHERE id = 1 AND token_hash = ? AND expires_at > ?)
             AND NOT EXISTS (SELECT 1 FROM admins)",
    )
    .bind(sha256(req.token.as_bytes()))
    .bind(now())
    .fetch_one(&state.db)
    .await?;
    if !token_valid {
        return Err(forbidden());
    }
    let password_hash = state.hasher.hash(req.password).await?;
    let hash_for_session = password_hash.clone();

    // Deleting the token is the first write, so a parallel setup waits here and then finds no token.
    let mut tx = state.db.begin().await?;
    let consumed = sqlx::query("DELETE FROM setup_token WHERE id = 1 AND token_hash = ? AND expires_at > ?")
        .bind(sha256(req.token.as_bytes()))
        .bind(now())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if consumed != 1 || admin_count_tx(&mut tx).await? > 0 {
        return Err(forbidden());
    }
    let admin_id: i64 =
        sqlx::query_scalar("INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, ?) RETURNING id")
            .bind(&req.username)
            .bind(password_hash)
            .bind(now())
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;

    tracing::info!(username = %req.username, "first admin created");
    let cookie = session::create(&state.db, admin_id, &hash_for_session, &ip, user_agent(&headers))
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    Ok((
        jar.add(cookie),
        Json(Me {
            id: admin_id,
            username: req.username,
        }),
    ))
}

async fn admin_count_tx(tx: &mut sqlx::SqliteConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM admins").fetch_one(tx).await
}

#[utoipa::path(post, path = "/auth/login", tag = "auth", request_body = LoginRequest, responses(
    (status = OK, body = Me),
    (status = UNAUTHORIZED, body = ErrorBody),
    (status = TOO_MANY_REQUESTS, body = ErrorBody),
))]
async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let invalid = || {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "The username or password is not correct.",
        )
    };
    let limits = &state.login;
    if !limits.attempts.try_record(&ip.to_string()) {
        return Err(ApiError::too_many_requests("Too many attempts. Wait a minute."));
    }
    let ip_key = format!("ip:{ip}");
    // Such a username cannot exist. The check also keeps long strings out of the limiter.
    if validate_username(&req.username).is_err() {
        limits.failures.try_record(&ip_key);
        return Err(invalid());
    }
    let user_key = format!("user:{}", req.username.to_lowercase());
    // Each attempt counts as a failure before the password check, so parallel attempts cannot
    // pass the limit. A successful login takes the count back.
    let ip_reserved = limits.failures.try_record(&ip_key);
    let user_reserved = limits.failures.try_record(&user_key);
    if !ip_reserved || !user_reserved {
        if ip_reserved {
            limits.failures.release(&ip_key);
        }
        if user_reserved {
            limits.failures.release(&user_key);
        }
        return Err(ApiError::too_many_requests("Too many failed logins. Wait 15 minutes."));
    }

    let admin: Option<(i64, String, String)> =
        sqlx::query_as("SELECT id, username, password_hash FROM admins WHERE username = ? AND disabled = 0")
            .bind(&req.username)
            .fetch_optional(&state.db)
            .await?;
    let hash = admin.as_ref().map(|(_, _, hash)| hash.clone());
    let valid = state.hasher.verify(hash, req.password).await?;
    let session = match admin.filter(|_| valid) {
        Some((admin_id, username, hash)) => {
            session::create(&state.db, admin_id, &hash, &ip.to_string(), user_agent(&headers))
                .await?
                .map(|cookie| (cookie, Me { id: admin_id, username }))
        }
        None => None,
    };
    let Some((cookie, me)) = session else {
        return Err(invalid());
    };

    limits.failures.release(&ip_key);
    limits.failures.clear(&user_key);
    Ok((jar.add(cookie), Json(me)))
}

#[utoipa::path(post, path = "/auth/logout", tag = "auth", responses((status = NO_CONTENT)))]
async fn logout(State(state): State<AppState>, jar: CookieJar) -> ApiResult<(CookieJar, StatusCode)> {
    session::delete_by_cookie(&state.db, &jar).await?;
    Ok((jar.remove(session::removal_cookie()), StatusCode::NO_CONTENT))
}

#[utoipa::path(get, path = "/auth/me", tag = "auth", responses(
    (status = OK, body = Me),
    (status = UNAUTHORIZED, body = ErrorBody),
))]
async fn me(session: AdminSession) -> Json<Me> {
    Json(Me {
        id: session.admin_id,
        username: session.username,
    })
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok())
}
