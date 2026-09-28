//! API key check and per-key limits for the gateway.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::error::{ErrorFormat, GatewayError};
use super::rejected::Reason;
use crate::crypto::sha256;
use crate::db::now;
use crate::state::AppState;

const DEFAULT_CONCURRENCY: usize = 8;
pub const GLOBAL_CONCURRENCY: usize = 64;
/// `last_used_at` is written at most this often per key.
const LAST_USED_INTERVAL_SECS: u64 = 60;

/// The API key of an admitted request. Handlers read it from the request extensions.
#[derive(Clone, Debug)]
pub struct ApiKey {
    pub id: i64,
    /// Model patterns; empty means all models.
    pub allowlist: Vec<String>,
}

/// Limits that live in memory: requests per minute and parallel requests, per key and in total.
pub struct KeyLimits {
    keys: Mutex<HashMap<i64, KeyState>>,
    global: Arc<Semaphore>,
}

struct KeyState {
    rpm: Option<TokenBucket>,
    tpm: Option<TokenBucket>,
    /// Requests of this key that are running now.
    active: Arc<AtomicUsize>,
    last_used_write: Option<Instant>,
}

/// Counts one running request of a key until it is dropped.
struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl KeyLimits {
    /// Reserves tokens before a request. Returns the seconds to wait when the key's
    /// tokens-per-minute budget is used up. A reservation is at most the whole budget, so a large
    /// request can pass when the budget is full; its real usage then delays the next requests.
    pub fn reserve_tokens(&self, key: i64, amount: i64) -> Result<(), u64> {
        let mut keys = self.keys.lock().expect("key limits lock");
        let Some(bucket) = keys.get_mut(&key).and_then(|state| state.tpm.as_mut()) else {
            return Ok(());
        };
        let amount = (amount as f64).min(bucket.capacity);
        bucket.take(amount)
    }

    /// Corrects a reservation with the real usage. The bucket may go below zero, which delays
    /// the next requests.
    pub fn settle_tokens(&self, key: i64, reserved: i64, used: i64) {
        let mut keys = self.keys.lock().expect("key limits lock");
        if let Some(bucket) = keys.get_mut(&key).and_then(|state| state.tpm.as_mut()) {
            bucket.tokens += (reserved - used) as f64;
            bucket.tokens = bucket.tokens.min(bucket.capacity);
        }
    }
}

impl Default for KeyLimits {
    fn default() -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
            global: Arc::new(Semaphore::new(GLOBAL_CONCURRENCY)),
        }
    }
}

/// Token bucket that refills `capacity` tokens per minute.
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    updated: Instant,
}

impl TokenBucket {
    fn new(per_minute: i64) -> Self {
        Self {
            capacity: per_minute as f64,
            tokens: per_minute as f64,
            updated: Instant::now(),
        }
    }

    /// A changed limit keeps the tokens that are left (at most the new capacity).
    fn set_capacity(&mut self, per_minute: i64) {
        self.refill();
        self.capacity = per_minute as f64;
        self.tokens = self.tokens.min(self.capacity);
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let refill = now.duration_since(self.updated).as_secs_f64() * self.capacity / 60.0;
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.updated = now;
    }

    /// Takes `amount` tokens, or returns the seconds until they are available.
    fn take(&mut self, amount: f64) -> Result<(), u64> {
        self.refill();
        if self.tokens >= amount {
            self.tokens -= amount;
            Ok(())
        } else {
            let missing = amount - self.tokens;
            Err((missing * 60.0 / self.capacity).ceil() as u64)
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    concurrency_limit: Option<i64>,
    allowlist: String,
}

/// The error format depends on the API the client speaks.
pub fn error_format(path: &str, headers: &HeaderMap) -> ErrorFormat {
    if path.starts_with("/v1/messages") || headers.contains_key("anthropic-version") {
        ErrorFormat::Anthropic
    } else {
        ErrorFormat::OpenAi
    }
}

/// Middleware for `/v1`: checks the key and the limits. The permits for parallel requests stay
/// with the response body, so a streamed answer counts until it ends.
pub async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let format = error_format(request.uri().path(), request.headers());
    let (key, permits) = match admit(&state, request.headers(), format).await {
        Ok(admitted) => admitted,
        Err((reason, error)) => {
            state.rejected.count(reason);
            return error.into_response();
        }
    };
    tracing::debug!(api_key = key.id, "request admitted");
    request.extensions_mut().insert(key);
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    let body = Body::from_stream(body.into_data_stream().map(move |chunk| {
        let _held = &permits;
        chunk
    }));
    Response::from_parts(parts, body)
}

type Rejection = (Reason, GatewayError);

async fn admit(
    state: &AppState,
    headers: &HeaderMap,
    format: ErrorFormat,
) -> Result<(ApiKey, (ActiveGuard, OwnedSemaphorePermit)), Rejection> {
    let unauthorized = |reason: Reason, message: &str| {
        (
            reason,
            GatewayError::new(format, StatusCode::UNAUTHORIZED, "invalid_api_key", message),
        )
    };
    let Some(presented) = presented_key(headers) else {
        return Err(unauthorized(
            Reason::NoKey,
            "No API key. Send it as `Authorization: Bearer <key>` or `x-api-key: <key>`.",
        ));
    };

    let row: Option<Row> = sqlx::query_as(
        "SELECT id, expires_at, revoked_at, rpm_limit, tpm_limit, concurrency_limit, allowlist
         FROM api_keys WHERE key_hash = ?",
    )
    .bind(sha256(presented.as_bytes()))
    .fetch_optional(&state.db)
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "database error in key check");
        (
            Reason::BadKey,
            GatewayError::new(
                format,
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Internal error.",
            ),
        )
    })?;
    let Some(row) = row else {
        return Err(unauthorized(Reason::BadKey, "The API key is not valid."));
    };
    if row.revoked_at.is_some() {
        return Err(unauthorized(Reason::Revoked, "The API key is revoked."));
    }
    if row.expires_at.is_some_and(|at| at <= now()) {
        return Err(unauthorized(Reason::Expired, "The API key is expired."));
    }

    let limited = |message: String, retry_after: u64| {
        (
            Reason::RateLimited,
            GatewayError::new(format, StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded", message)
                .with_retry_after(retry_after),
        )
    };
    let global = state
        .key_limits
        .global
        .clone()
        .try_acquire_owned()
        .map_err(|_| limited("The gateway is busy. Try again in a moment.".into(), 1))?;
    let (per_key, write_last_used) = {
        let mut keys = state.key_limits.keys.lock().expect("key limits lock");
        let concurrency = row.concurrency_limit.map_or(DEFAULT_CONCURRENCY, |limit| {
            usize::try_from(limit).unwrap_or(DEFAULT_CONCURRENCY)
        });
        let entry = keys.entry(row.id).or_insert_with(|| KeyState {
            rpm: None,
            tpm: None,
            active: Arc::default(),
            last_used_write: None,
        });
        // The limits come from the database on each request, so a change takes effect at once.
        match (entry.rpm.as_mut(), row.rpm_limit) {
            (Some(bucket), Some(limit)) => bucket.set_capacity(limit),
            (None, Some(limit)) => entry.rpm = Some(TokenBucket::new(limit)),
            (_, None) => entry.rpm = None,
        }
        match (entry.tpm.as_mut(), row.tpm_limit) {
            (Some(bucket), Some(limit)) => bucket.set_capacity(limit),
            (None, Some(limit)) => entry.tpm = Some(TokenBucket::new(limit)),
            (_, None) => entry.tpm = None,
        }
        if entry.active.load(Ordering::SeqCst) >= concurrency {
            return Err(limited(
                format!("This key has {concurrency} requests running. Wait for one to end."),
                1,
            ));
        }
        if let Some(bucket) = entry.rpm.as_mut() {
            bucket
                .take(1.0)
                .map_err(|wait| limited("This key sent too many requests per minute.".into(), wait))?;
        }
        entry.active.fetch_add(1, Ordering::SeqCst);
        let guard = ActiveGuard(entry.active.clone());
        let write = entry
            .last_used_write
            .is_none_or(|at| at.elapsed().as_secs() >= LAST_USED_INTERVAL_SECS);
        if write {
            entry.last_used_write = Some(Instant::now());
        }
        (guard, write)
    };

    if write_last_used {
        let result = sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
            .bind(now())
            .bind(row.id)
            .execute(&state.db)
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "cannot save last use of an API key");
        }
    }

    let key = ApiKey {
        id: row.id,
        allowlist: serde_json::from_str(&row.allowlist).unwrap_or_default(),
    };
    Ok((key, (per_key, global)))
}

/// Reads the key from `Authorization: Bearer` or `x-api-key`.
fn presented_key(headers: &HeaderMap) -> Option<String> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, key)| key);
    let x_api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    bearer
        .or(x_api_key)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}
