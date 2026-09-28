//! Passkeys (WebAuthn): add and remove, use as second factor, or log in without a password.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use webauthn_rs::prelude::{
    CreationChallengeResponse, DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyAuthentication,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Uuid,
};
use webauthn_rs_proto::ResidentKeyRequirement;

use super::auth::{Me, user_agent};
use super::factors::{self, FactorAdded};
use super::session::{self, AdminSession, ClientInfo, Kind, PendingSession, Proof};
use crate::client_ip::ClientIp;
use crate::crypto::random_token;
use crate::db::now;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;

const CEREMONY_COOKIE: &str = "__Host-aiproxy_passkey";
const CEREMONY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_CEREMONIES: usize = 10_000;
const MAX_NAME: usize = 64;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(registration_options))
        .routes(routes!(register))
        .routes(routes!(delete_passkey))
        .routes(routes!(second_factor_options))
        .routes(routes!(verify_second_factor))
        .routes(routes!(passkey_login_options))
        .routes(routes!(passkey_login))
}

#[derive(Serialize, sqlx::FromRow, ToSchema)]
pub struct PasskeyView {
    id: i64,
    name: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

pub async fn list(db: &SqlitePool, admin_id: i64) -> Result<Vec<PasskeyView>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM admin_passkeys WHERE admin_id = ? ORDER BY created_at",
    )
    .bind(admin_id)
    .fetch_all(db)
    .await
}

// ---------------------------------------------------------------------------------------------
// Ceremony state

/// Server-side state of the WebAuthn ceremonies that are in progress. Each state is used once
/// and is valid for 5 minutes. It is in memory only; a restart cancels open ceremonies.
#[derive(Default)]
pub struct Ceremonies {
    entries: Mutex<HashMap<String, (Instant, Ceremony)>>,
}

enum Ceremony {
    Register {
        session_id: i64,
        state: PasskeyRegistration,
    },
    SecondFactor {
        session_id: i64,
        state: PasskeyAuthentication,
    },
    Login {
        state: DiscoverableAuthentication,
    },
}

impl Ceremony {
    fn session_id(&self) -> Option<i64> {
        match self {
            Self::Register { session_id, .. } | Self::SecondFactor { session_id, .. } => Some(*session_id),
            Self::Login { .. } => None,
        }
    }
}

impl Ceremonies {
    /// Stores the state. A session has at most one open ceremony: a new one replaces the old one.
    fn insert(&self, ceremony: Ceremony) -> Result<String, ApiError> {
        let mut entries = self.entries.lock().expect("ceremony lock");
        let session = ceremony.session_id();
        entries.retain(|_, (started, old)| {
            started.elapsed() < CEREMONY_TIMEOUT && (session.is_none() || old.session_id() != session)
        });
        if entries.len() >= MAX_CEREMONIES {
            return Err(ApiError::too_many_requests(
                "Too many passkey requests. Try again in a moment.",
            ));
        }
        let id = random_token(24);
        entries.insert(id.clone(), (Instant::now(), ceremony));
        Ok(id)
    }

    /// Removes and returns the state, so it can be used only once.
    fn take(&self, id: &str) -> Option<Ceremony> {
        let mut entries = self.entries.lock().expect("ceremony lock");
        entries
            .remove(id)
            .filter(|(started, _)| started.elapsed() < CEREMONY_TIMEOUT)
            .map(|(_, ceremony)| ceremony)
    }
}

/// Passkeys need a host name (not an IP address) in the public URL.
fn webauthn(state: &AppState) -> ApiResult<&webauthn_rs::Webauthn> {
    state
        .webauthn
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("Passkeys need a host name in AIPROXY_PUBLIC_URL, not an IP address."))
}

fn ceremony_expired() -> ApiError {
    ApiError::bad_request("The passkey request expired. Start again.")
}

fn passkey_failed(err: impl std::fmt::Display) -> ApiError {
    tracing::info!(error = %err, "passkey check failed");
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "passkey_failed",
        "The passkey was not accepted.",
    )
}

/// WebAuthn options and credentials are passed through as JSON objects.
#[derive(Serialize, ToSchema)]
pub struct Options {
    /// Id of this ceremony, to send back with the answer.
    ceremony: String,
    /// Options for `navigator.credentials` (`publicKey` member).
    #[schema(value_type = Value)]
    options: serde_json::Value,
}

fn options_json(value: impl Serialize) -> ApiResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|err| {
        tracing::error!(error = %err, "cannot serialize WebAuthn options");
        ApiError::internal()
    })
}

// ---------------------------------------------------------------------------------------------
// Register a passkey

#[utoipa::path(post, path = "/account/passkeys/options", tag = "account", responses((status = OK, body = Options)))]
async fn registration_options(current: AdminSession, State(state): State<AppState>) -> ApiResult<Json<Options>> {
    let user_id = webauthn_user_id(&state.db, current.admin_id).await?;
    let existing = stored_passkeys(&state.db, current.admin_id)
        .await?
        .into_iter()
        .map(|stored| stored.passkey.cred_id().clone())
        .collect();
    let (mut challenge, registration): (CreationChallengeResponse, PasskeyRegistration) = webauthn(&state)?
        .start_passkey_registration(user_id, &current.username, &current.username, Some(existing))
        .map_err(|err| {
            tracing::error!(error = %err, "cannot start passkey registration");
            ApiError::internal()
        })?;
    // Passwordless login needs a discoverable credential.
    if let Some(selection) = challenge.public_key.authenticator_selection.as_mut() {
        selection.resident_key = Some(ResidentKeyRequirement::Required);
        selection.require_resident_key = true;
    }
    let ceremony = state.ceremonies.insert(Ceremony::Register {
        session_id: current.session_id,
        state: registration,
    })?;
    Ok(Json(Options {
        ceremony,
        options: options_json(challenge.public_key)?,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct RegisterRequest {
    ceremony: String,
    name: String,
    #[schema(value_type = Value)]
    credential: serde_json::Value,
}

#[utoipa::path(post, path = "/account/passkeys", tag = "account", request_body = RegisterRequest, responses(
    (status = OK, body = FactorAdded),
    (status = BAD_REQUEST, body = ErrorBody),
))]
async fn register(
    current: AdminSession,
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<FactorAdded>> {
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(ApiError::bad_request("The name must have 1 to 64 characters."));
    }
    let Some(Ceremony::Register {
        session_id,
        state: registration,
    }) = state.ceremonies.take(&req.ceremony)
    else {
        return Err(ceremony_expired());
    };
    if session_id != current.session_id {
        return Err(ceremony_expired());
    }
    let credential: RegisterPublicKeyCredential = serde_json::from_value(req.credential)
        .map_err(|_| ApiError::bad_request("The passkey answer is not valid."))?;
    let passkey = webauthn(&state)?
        .finish_passkey_registration(&credential, &registration)
        .map_err(passkey_failed)?;

    // The session must still exist: a factor reset at the same time ends it.
    let inserted = sqlx::query(
        "INSERT INTO admin_passkeys (admin_id, name, credential_id, credential, created_at)
         SELECT ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND pending_second_factor = 0)",
    )
    .bind(current.admin_id)
    .bind(name)
    .bind(passkey.cred_id().as_ref())
    .bind(serde_json::to_string(&passkey).map_err(|_| ApiError::internal())?)
    .bind(now())
    .bind(current.session_id)
    .execute(&state.db)
    .await
    .map_err(|err| match err {
        sqlx::Error::Database(db) if db.is_unique_violation() => ApiError::conflict("This passkey is already added."),
        other => other.into(),
    })?
    .rows_affected();
    if inserted == 0 {
        return Err(ApiError::unauthorized());
    }
    Ok(Json(FactorAdded {
        recovery_codes: factors::first_recovery_codes(&state.db, current.admin_id, current.session_id).await?,
    }))
}

#[utoipa::path(delete, path = "/account/passkeys/{id}", tag = "account", responses(
    (status = NO_CONTENT),
    (status = NOT_FOUND, body = ErrorBody),
))]
async fn delete_passkey(
    current: AdminSession,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM admin_passkeys WHERE id = ? AND admin_id = ?")
        .bind(id)
        .bind(current.admin_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::not_found("The passkey does not exist."));
    }
    factors::after_factor_removed(&state.db, current.admin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The WebAuthn user handle of the admin. It is created with the first passkey.
async fn webauthn_user_id(db: &SqlitePool, admin_id: i64) -> ApiResult<Uuid> {
    let new_id = Uuid::new_v4().to_string();
    sqlx::query("UPDATE admins SET webauthn_id = ? WHERE id = ? AND webauthn_id IS NULL")
        .bind(&new_id)
        .bind(admin_id)
        .execute(db)
        .await?;
    let id: String = sqlx::query_scalar("SELECT webauthn_id FROM admins WHERE id = ?")
        .bind(admin_id)
        .fetch_one(db)
        .await?;
    Uuid::parse_str(&id).map_err(|_| ApiError::internal())
}

/// A stored passkey: row id, the stored JSON, and the parsed credential.
struct Stored {
    id: i64,
    json: String,
    passkey: Passkey,
}

async fn stored_passkeys(db: &SqlitePool, admin_id: i64) -> ApiResult<Vec<Stored>> {
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, credential FROM admin_passkeys WHERE admin_id = ?")
        .bind(admin_id)
        .fetch_all(db)
        .await?;
    rows.into_iter()
        .map(|(id, json)| {
            serde_json::from_str(&json)
                .map(|passkey| Stored { id, json, passkey })
                .map_err(|err| {
                    tracing::error!(error = %err, passkey = id, "stored passkey is not readable");
                    ApiError::internal()
                })
        })
        .collect()
}

/// Checks the sign counter against the current stored credential, then saves the new counter and
/// the last use. A counter that does not go up points to a cloned authenticator (or a parallel
/// login), so the login is refused. The update is also refused when another login changed the
/// stored credential in the meantime.
async fn record_use(
    db: &SqlitePool,
    stored: Stored,
    result: &webauthn_rs::prelude::AuthenticationResult,
) -> ApiResult<()> {
    let Stored { id, json, mut passkey } = stored;
    let stored_counter = serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|value| value["cred"]["counter"].as_u64())
        .unwrap_or(0);
    // Authenticators without a counter always send 0.
    if (result.counter() > 0 || stored_counter > 0) && u64::from(result.counter()) <= stored_counter {
        return Err(passkey_failed("the sign counter did not go up"));
    }
    passkey.update_credential(result);
    let updated =
        sqlx::query("UPDATE admin_passkeys SET credential = ?, last_used_at = ? WHERE id = ? AND credential = ?")
            .bind(serde_json::to_string(&passkey).map_err(|_| ApiError::internal())?)
            .bind(now())
            .bind(id)
            .bind(json)
            .execute(db)
            .await?
            .rows_affected();
    if updated == 0 {
        return Err(passkey_failed("passkey changed during the login"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Passkey as second factor

#[utoipa::path(post, path = "/auth/second-factor/passkey/options", tag = "auth", responses(
    (status = OK, body = Options),
    (status = UNAUTHORIZED, body = ErrorBody),
))]
async fn second_factor_options(pending: PendingSession, State(state): State<AppState>) -> ApiResult<Json<Options>> {
    let passkeys: Vec<Passkey> = stored_passkeys(&state.db, pending.admin_id)
        .await?
        .into_iter()
        .map(|stored| stored.passkey)
        .collect();
    if passkeys.is_empty() {
        return Err(ApiError::bad_request("No passkey is added for this admin."));
    }
    let (challenge, authentication): (RequestChallengeResponse, PasskeyAuthentication) = webauthn(&state)?
        .start_passkey_authentication(&passkeys)
        .map_err(|err| {
            tracing::error!(error = %err, "cannot start passkey authentication");
            ApiError::internal()
        })?;
    let ceremony = state.ceremonies.insert(Ceremony::SecondFactor {
        session_id: pending.session_id,
        state: authentication,
    })?;
    Ok(Json(Options {
        ceremony,
        options: options_json(challenge.public_key)?,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct AssertionRequest {
    ceremony: String,
    #[schema(value_type = Value)]
    credential: serde_json::Value,
}

#[utoipa::path(post, path = "/auth/second-factor/passkey", tag = "auth", request_body = AssertionRequest, responses(
    (status = OK, body = Me),
    (status = UNAUTHORIZED, body = ErrorBody),
    (status = TOO_MANY_REQUESTS, body = ErrorBody),
))]
async fn verify_second_factor(
    pending: PendingSession,
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<AssertionRequest>,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let key = factors::limit_key(pending.admin_id);
    factors::reserve_attempt(&state, &key)?;
    let Some(Ceremony::SecondFactor {
        session_id,
        state: authentication,
    }) = state.ceremonies.take(&req.ceremony)
    else {
        return Err(ceremony_expired());
    };
    if session_id != pending.session_id {
        return Err(ceremony_expired());
    }
    let credential: PublicKeyCredential = serde_json::from_value(req.credential)
        .map_err(|_| ApiError::bad_request("The passkey answer is not valid."))?;
    let result = webauthn(&state)?
        .finish_passkey_authentication(&credential, &authentication)
        .map_err(passkey_failed)?;
    let stored = stored_passkeys(&state.db, pending.admin_id)
        .await?
        .into_iter()
        .find(|stored| stored.passkey.cred_id() == result.cred_id())
        .ok_or_else(|| passkey_failed("passkey was removed"))?;
    record_use(&state.db, stored, &result).await?;
    state.login.failures.release(&key);
    factors::complete_login(&state, pending, &ip.to_string(), &headers, jar).await
}

// ---------------------------------------------------------------------------------------------
// Log in with a passkey only

#[utoipa::path(post, path = "/auth/passkey/options", tag = "auth", responses(
    (status = OK, body = Options),
    (status = TOO_MANY_REQUESTS, body = ErrorBody),
))]
async fn passkey_login_options(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
) -> ApiResult<(CookieJar, Json<Options>)> {
    if !state.login.attempts.try_record(&ip.to_string()) {
        return Err(ApiError::too_many_requests("Too many attempts. Wait a minute."));
    }
    let (mut challenge, authentication) = webauthn(&state)?.start_discoverable_authentication().map_err(|err| {
        tracing::error!(error = %err, "cannot start passkey login");
        ApiError::internal()
    })?;
    // The login button shows the normal passkey dialog, not the autofill variant.
    challenge.mediation = None;
    let ceremony = state.ceremonies.insert(Ceremony::Login { state: authentication })?;
    // The ceremony id is also in a cookie, so only this browser can finish the login.
    let cookie = Cookie::build((CEREMONY_COOKIE, ceremony.clone()))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .path("/")
        .max_age(time::Duration::seconds(CEREMONY_TIMEOUT.as_secs() as i64))
        .build();
    Ok((
        jar.add(cookie),
        Json(Options {
            ceremony,
            options: options_json(challenge.public_key)?,
        }),
    ))
}

#[utoipa::path(post, path = "/auth/passkey", tag = "auth", request_body = AssertionRequest, responses(
    (status = OK, body = Me),
    (status = UNAUTHORIZED, body = ErrorBody),
))]
async fn passkey_login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<AssertionRequest>,
) -> ApiResult<(CookieJar, Json<Me>)> {
    let same_browser = jar.get(CEREMONY_COOKIE).is_some_and(|c| c.value() == req.ceremony);
    let jar = jar.remove(Cookie::build((CEREMONY_COOKIE, "")).path("/").secure(true).build());
    let Some(Ceremony::Login { state: authentication }) = state.ceremonies.take(&req.ceremony) else {
        return Err(ceremony_expired());
    };
    if !same_browser {
        return Err(ceremony_expired());
    }
    let credential: PublicKeyCredential = serde_json::from_value(req.credential)
        .map_err(|_| ApiError::bad_request("The passkey answer is not valid."))?;
    let (user_id, credential_id) = webauthn(&state)?
        .identify_discoverable_authentication(&credential)
        .map_err(passkey_failed)?;
    let admin: Option<(i64, String)> =
        sqlx::query_as("SELECT id, username FROM admins WHERE webauthn_id = ? AND disabled = 0")
            .bind(user_id.to_string())
            .fetch_optional(&state.db)
            .await?;
    let (admin_id, username) = admin.ok_or_else(|| passkey_failed("unknown user handle"))?;
    let stored = stored_passkeys(&state.db, admin_id)
        .await?
        .into_iter()
        .find(|stored| stored.passkey.cred_id().as_ref() == credential_id)
        .ok_or_else(|| passkey_failed("unknown credential"))?;
    let result = webauthn(&state)?
        .finish_discoverable_authentication(&credential, authentication, &[DiscoverableKey::from(&stored.passkey)])
        .map_err(passkey_failed)?;
    let id = stored.id;
    record_use(&state.db, stored, &result).await?;

    let client = ClientInfo {
        ip: &ip.to_string(),
        user_agent: user_agent(&headers),
    };
    let cookie = session::create(&state.db, admin_id, Proof::Passkey(id), Kind::Full, client)
        .await?
        .ok_or_else(|| passkey_failed("passkey or admin changed"))?;
    Ok((jar.add(cookie), Json(Me { id: admin_id, username })))
}
