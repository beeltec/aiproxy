//! Second factors: TOTP and recovery codes, and the second login step.
//! Passkeys are in `passkeys.rs`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use totp_rs::{Builder, Totp};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Me, user_agent};
use super::session::{self, AdminSession, ClientInfo, Kind, PendingSession, Proof};
use crate::client_ip::ClientIp;
use crate::crypto::{random_bytes, sha256};
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;

const RECOVERY_CODES: usize = 10;
const ISSUER: &str = "aiproxy";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(pending_methods))
        .routes(routes!(verify_totp))
        .routes(routes!(verify_recovery_code))
        .routes(routes!(list_factors))
        .routes(routes!(start_totp, delete_totp))
        .routes(routes!(confirm_totp))
        .routes(routes!(new_recovery_codes))
        .routes(routes!(reset_factors))
}

/// The second factors an admin can use.
#[derive(Serialize, ToSchema)]
pub struct Methods {
    pub totp: bool,
    pub passkey: bool,
    pub recovery_code: bool,
}

pub async fn methods(db: &SqlitePool, admin_id: i64) -> Result<Methods, sqlx::Error> {
    let (totp, passkey, recovery_code): (bool, bool, bool) = sqlx::query_as(
        "SELECT
             EXISTS (SELECT 1 FROM admin_totp WHERE admin_id = ?1 AND confirmed = 1),
             EXISTS (SELECT 1 FROM admin_passkeys WHERE admin_id = ?1),
             EXISTS (SELECT 1 FROM admin_recovery_codes WHERE admin_id = ?1 AND used_at IS NULL)",
    )
    .bind(admin_id)
    .fetch_one(db)
    .await?;
    Ok(Methods {
        totp,
        passkey,
        recovery_code,
    })
}

impl Methods {
    /// True when a password alone is not enough to log in.
    pub fn any(&self) -> bool {
        self.totp || self.passkey
    }
}

// ---------------------------------------------------------------------------------------------
// Second login step

#[derive(Deserialize, ToSchema)]
pub struct CodeRequest {
    code: String,
}

/// The second factors for the pending login.
#[utoipa::path(get, path = "/auth/second-factor", tag = "auth", responses(
    (status = OK, body = Methods),
    (status = UNAUTHORIZED, body = ErrorBody),
))]
async fn pending_methods(pending: PendingSession, State(state): State<AppState>) -> ApiResult<Json<Methods>> {
    Ok(Json(methods(&state.db, pending.admin_id).await?))
}

#[utoipa::path(post, path = "/auth/second-factor/totp", tag = "auth", request_body = CodeRequest, responses(
    (status = OK, body = Me),
    (status = UNAUTHORIZED, body = ErrorBody),
    (status = TOO_MANY_REQUESTS, body = ErrorBody),
))]
async fn verify_totp(
    pending: PendingSession,
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<CodeRequest>,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let key = limit_key(pending.admin_id);
    reserve_attempt(&state, &key)?;
    if !check_totp(&state, pending.admin_id, &pending.username, &req.code, TotpUse::Login).await? {
        return Err(wrong_code());
    }
    state.login.failures.release(&key);
    complete_login(&state, pending, &ip.to_string(), &headers, jar).await
}

#[utoipa::path(post, path = "/auth/second-factor/recovery-code", tag = "auth", request_body = CodeRequest, responses(
    (status = OK, body = Me),
    (status = UNAUTHORIZED, body = ErrorBody),
    (status = TOO_MANY_REQUESTS, body = ErrorBody),
))]
async fn verify_recovery_code(
    pending: PendingSession,
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<CodeRequest>,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let key = limit_key(pending.admin_id);
    reserve_attempt(&state, &key)?;
    let used = sqlx::query(
        "UPDATE admin_recovery_codes SET used_at = ? WHERE admin_id = ? AND code_hash = ? AND used_at IS NULL",
    )
    .bind(now())
    .bind(pending.admin_id)
    .bind(recovery_code_hash(&req.code))
    .execute(&state.db)
    .await?
    .rows_affected();
    if used == 0 {
        return Err(wrong_code());
    }
    state.login.failures.release(&key);
    complete_login(&state, pending, &ip.to_string(), &headers, jar).await
}

/// Turns the pending session into a full session with a new token.
pub async fn complete_login(
    state: &AppState,
    pending: PendingSession,
    ip: &str,
    headers: &HeaderMap,
    jar: CookieJar,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let client = ClientInfo {
        ip,
        user_agent: user_agent(headers),
    };
    let cookie = session::create(
        &state.db,
        pending.admin_id,
        Proof::Pending(pending.session_id),
        Kind::Full,
        client,
    )
    .await?
    .ok_or_else(ApiError::unauthorized)?;
    session::delete(&state.db, pending.session_id).await?;
    Ok((
        jar.add(cookie),
        Json(Me {
            id: pending.admin_id,
            username: pending.username,
        }),
    ))
}

/// Failed second-factor checks share the login failure limit, per admin.
pub fn limit_key(admin_id: i64) -> String {
    format!("second-factor:{admin_id}")
}

/// Counts the attempt as a failure in advance, so parallel attempts cannot pass the limit.
/// A successful check takes it back.
pub fn reserve_attempt(state: &AppState, key: &str) -> ApiResult<()> {
    if state.login.failures.try_record(key) {
        Ok(())
    } else {
        Err(ApiError::too_many_requests("Too many wrong codes. Wait 15 minutes."))
    }
}

fn wrong_code() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "wrong_code", "The code is not correct.")
}

// ---------------------------------------------------------------------------------------------
// TOTP

fn totp_aad(admin_id: i64) -> String {
    format!("admin_totp:{admin_id}")
}

fn totp(secret: Vec<u8>, username: &str) -> anyhow::Result<Totp> {
    Builder::new()
        .with_secret(secret)
        .with_account_name(username)
        .with_issuer(Some(ISSUER))
        .build()
        .map_err(|err| anyhow::anyhow!("invalid TOTP configuration: {err}"))
}

/// What a correct TOTP code does.
#[derive(Clone, Copy)]
enum TotpUse {
    /// Second login step with the active secret.
    Login,
    /// Confirms the new secret and turns TOTP on.
    Confirm,
}

/// Checks a code. A time step is accepted only once. The update is bound to the exact secret
/// that was checked, so a secret that another request stored in the meantime is not affected.
async fn check_totp(state: &AppState, admin_id: i64, username: &str, code: &str, usage: TotpUse) -> ApiResult<bool> {
    let confirmed = matches!(usage, TotpUse::Login);
    let stored: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT secret_enc FROM admin_totp WHERE admin_id = ? AND confirmed = ?")
            .bind(admin_id)
            .bind(confirmed)
            .fetch_optional(&state.db)
            .await?;
    let Some(stored) = stored else {
        return Ok(false);
    };
    let secret = state.secrets.decrypt(&totp_aad(admin_id), &stored).map_err(internal)?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(step) = totp(secret, username).map_err(internal)?.check_current(&code) else {
        return Ok(false);
    };
    let step = i64::try_from(step).map_err(|_| ApiError::internal())?;
    let accepted = sqlx::query(
        "UPDATE admin_totp SET last_used_step = ?1, confirmed = 1
         WHERE admin_id = ?2 AND secret_enc = ?3 AND confirmed = ?4 AND last_used_step < ?1",
    )
    .bind(step)
    .bind(admin_id)
    .bind(&stored)
    .bind(confirmed)
    .execute(&state.db)
    .await?
    .rows_affected();
    Ok(accepted == 1)
}

fn internal(err: anyhow::Error) -> ApiError {
    tracing::error!(error = %err, "second factor error");
    ApiError::internal()
}

#[derive(Serialize, ToSchema)]
pub struct Factors {
    totp: bool,
    passkeys: Vec<super::passkeys::PasskeyView>,
    recovery_codes_left: i64,
}

#[utoipa::path(get, path = "/account/second-factors", tag = "account", responses((status = OK, body = Factors)))]
async fn list_factors(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Factors>> {
    let totp: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM admin_totp WHERE admin_id = ? AND confirmed = 1)")
            .bind(current.admin_id)
            .fetch_one(&state.db)
            .await?;
    let recovery_codes_left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM admin_recovery_codes WHERE admin_id = ? AND used_at IS NULL")
            .bind(current.admin_id)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(Factors {
        totp,
        passkeys: super::passkeys::list(&state.db, current.admin_id).await?,
        recovery_codes_left,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct TotpSetup {
    /// Base32 secret for manual entry.
    secret: String,
    otpauth_url: String,
    /// PNG image of the QR code, base64.
    qr_png_base64: String,
}

/// Starts TOTP setup with a new secret. The secret is active only after confirmation.
#[utoipa::path(post, path = "/account/totp", tag = "account", responses(
    (status = OK, body = TotpSetup),
    (status = CONFLICT, body = ErrorBody, description = "TOTP is already active"),
))]
async fn start_totp(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<TotpSetup>> {
    let secret = random_bytes(20);
    let totp = totp(secret.clone(), &current.username).map_err(internal)?;
    let setup = TotpSetup {
        secret: totp.secret().to_base32(),
        otpauth_url: totp.to_url().map_err(|e| internal(anyhow::anyhow!("{e}")))?,
        qr_png_base64: totp.to_qr_base64().map_err(|e| internal(anyhow::anyhow!("{e}")))?,
    };
    let stored = sqlx::query(
        "INSERT INTO admin_totp (admin_id, secret_enc, confirmed, last_used_step, created_at) VALUES (?, ?, 0, 0, ?)
         ON CONFLICT (admin_id) DO UPDATE SET secret_enc = excluded.secret_enc,
             last_used_step = 0, created_at = excluded.created_at
         WHERE admin_totp.confirmed = 0",
    )
    .bind(current.admin_id)
    .bind(state.secrets.encrypt(&totp_aad(current.admin_id), &secret))
    .bind(now())
    .execute(&state.db)
    .await?
    .rows_affected();
    if stored == 0 {
        return Err(ApiError::conflict("TOTP is already active. Remove it first."));
    }
    Ok(Json(setup))
}

#[derive(Serialize, ToSchema)]
pub struct FactorAdded {
    /// New recovery codes, only when this is the first second factor. Show them once.
    pub recovery_codes: Option<Vec<String>>,
}

#[utoipa::path(post, path = "/account/totp/confirm", tag = "account", request_body = CodeRequest, responses(
    (status = OK, body = FactorAdded),
    (status = BAD_REQUEST, body = ErrorBody, description = "The code is wrong"),
))]
async fn confirm_totp(
    current: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<CodeRequest>,
) -> ApiResult<Json<FactorAdded>> {
    if !check_totp(&state, current.admin_id, &current.username, &req.code, TotpUse::Confirm).await? {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "wrong_code",
            "The code is not correct. Check the time on your device.",
        ));
    }
    Ok(Json(FactorAdded {
        recovery_codes: first_recovery_codes(&state.db, current.admin_id).await?,
    }))
}

#[utoipa::path(delete, path = "/account/totp", tag = "account", responses((status = NO_CONTENT)))]
async fn delete_totp(current: AdminSession, State(state): State<AppState>) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM admin_totp WHERE admin_id = ?")
        .bind(current.admin_id)
        .execute(&state.db)
        .await?;
    after_factor_removed(&state.db, current.admin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Recovery codes

fn recovery_code_hash(code: &str) -> Vec<u8> {
    let normalized: String = code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    sha256(normalized.as_bytes())
}

fn generate_recovery_codes() -> Vec<String> {
    // 32 characters, so each random byte maps without bias. No "l", "o", "0" or "1".
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    (0..RECOVERY_CODES)
        .map(|_| {
            let chars: String = random_bytes(10)
                .iter()
                .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
                .collect();
            format!("{}-{}", &chars[..5], &chars[5..])
        })
        .collect()
}

/// Replaces all recovery codes of the admin with new ones and returns them.
async fn replace_recovery_codes(db: &SqlitePool, admin_id: i64) -> Result<Vec<String>, sqlx::Error> {
    let codes = generate_recovery_codes();
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM admin_recovery_codes WHERE admin_id = ?")
        .bind(admin_id)
        .execute(&mut *tx)
        .await?;
    for code in &codes {
        sqlx::query("INSERT INTO admin_recovery_codes (admin_id, code_hash) VALUES (?, ?)")
            .bind(admin_id)
            .bind(recovery_code_hash(code))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(codes)
}

/// Creates recovery codes after a second factor was added, if the admin has none yet.
/// The first insert decides it: a parallel request waits for this write and then finds codes.
pub async fn first_recovery_codes(db: &SqlitePool, admin_id: i64) -> Result<Option<Vec<String>>, sqlx::Error> {
    let codes = generate_recovery_codes();
    let mut tx = db.begin().await?;
    for (index, code) in codes.iter().enumerate() {
        let inserted = sqlx::query(
            "INSERT INTO admin_recovery_codes (admin_id, code_hash)
             SELECT ?1, ?2 WHERE ?3 OR NOT EXISTS (SELECT 1 FROM admin_recovery_codes WHERE admin_id = ?1)",
        )
        .bind(admin_id)
        .bind(recovery_code_hash(code))
        .bind(index > 0)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Ok(None);
        }
    }
    tx.commit().await?;
    Ok(Some(codes))
}

/// Recovery codes are useless without a second factor, so they go with the last one.
pub async fn after_factor_removed(db: &SqlitePool, admin_id: i64) -> Result<(), sqlx::Error> {
    if !methods(db, admin_id).await?.any() {
        sqlx::query("DELETE FROM admin_recovery_codes WHERE admin_id = ?")
            .bind(admin_id)
            .execute(db)
            .await?;
    }
    Ok(())
}

#[derive(Serialize, ToSchema)]
pub struct RecoveryCodes {
    recovery_codes: Vec<String>,
}

/// Replaces the recovery codes. The old codes stop working.
#[utoipa::path(post, path = "/account/recovery-codes", tag = "account", responses(
    (status = OK, body = RecoveryCodes),
    (status = CONFLICT, body = ErrorBody, description = "No second factor is active"),
))]
async fn new_recovery_codes(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<RecoveryCodes>> {
    if !methods(&state.db, current.admin_id).await?.any() {
        return Err(ApiError::conflict("Add a second factor first."));
    }
    Ok(Json(RecoveryCodes {
        recovery_codes: replace_recovery_codes(&state.db, current.admin_id).await?,
    }))
}

// ---------------------------------------------------------------------------------------------
// Reset by another admin

/// Removes all second factors of an admin and ends the sessions of that admin.
#[utoipa::path(post, path = "/admins/{id}/second-factors/reset", tag = "admins", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn reset_factors(
    current: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM admins WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("The admin does not exist."));
    }
    for sql in [
        "DELETE FROM admin_totp WHERE admin_id = ?",
        "DELETE FROM admin_passkeys WHERE admin_id = ?",
        "DELETE FROM admin_recovery_codes WHERE admin_id = ?",
    ] {
        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
    }
    let keep = (id == current.admin_id).then_some(current.session_id);
    session::delete_all_of(&mut *tx, id, keep).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
