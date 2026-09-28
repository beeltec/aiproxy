//! Runs one gateway request against the ChatGPT backend. The internal format is the OpenAI
//! Responses API: requests arrive as a Responses body, and the stream is Responses events.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::auth::{ApiKey, Permits};
use super::codex::{self, SendError};
use super::routing::Route;
use crate::chatgpt::refresh::{self, Trigger};
use crate::chatgpt::select::{self, Selection};
use crate::crypto::random_token;
use crate::db::now;
use crate::state::AppState;
use crate::usage::{Row, Tokens};

/// Longest time without any upstream event.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest time for one request.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Events held before the attempt commits (then it commits anyway).
const MAX_HELD_EVENTS: usize = 1000;
const MAX_HELD_BYTES: usize = 1024 * 1024;
/// Upper bound for everything one upstream answer may send.
const MAX_UPSTREAM_BYTES: usize = 64 * 1024 * 1024;
/// Refresh the access token when it expires within this time.
const REFRESH_MARGIN: i64 = 5 * 60;

/// One Responses stream event.
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: String,
    pub data: Value,
}

/// An error for the client. The handler adds the error format.
#[derive(Clone, Debug)]
pub struct Failure {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl Failure {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }
}

/// What the engine sends to the handler after the upstream accepted the request.
pub enum Msg {
    Event(Event),
    /// The final response object (with all output items).
    Done(Value),
    /// A failure after the start; the client already has a success status.
    Failed(Failure),
}

pub struct Job {
    pub key: ApiKey,
    pub route: Route,
    /// Responses request body from the client (before the backend adjustments).
    pub body: Value,
    pub route_name: &'static str,
    pub client_format: &'static str,
    pub stream: bool,
    pub cache_key: String,
    /// Tokens reserved for the tokens-per-minute limit.
    pub reserved_tokens: i64,
    /// Concurrency slots. They stay taken until the engine ends.
    pub permits: Option<Permits>,
}

/// Starts the request. Returns when the upstream accepted it, or with the error to send as
/// the HTTP answer. The work runs in its own task. A closed client stops the upstream request,
/// and the usage is still recorded.
pub async fn start(state: &AppState, job: Job) -> Result<mpsc::Receiver<Msg>, Failure> {
    let (opened_tx, opened_rx) = oneshot::channel();
    let (tx, rx) = mpsc::channel(64);
    let state = state.clone();
    tokio::spawn(run(state, job, opened_tx, tx));
    match opened_rx.await {
        Ok(Ok(())) => Ok(rx),
        Ok(Err(failure)) => Err(failure),
        Err(_) => Err(Failure::new(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "The request stopped.",
        )),
    }
}

#[derive(Default)]
struct Outcome {
    status: u16,
    error_kind: Option<String>,
    account: Option<i64>,
    final_response: Option<Value>,
    first_token_ms: Option<i64>,
    streamed_chars: usize,
    attempts: i64,
    /// True after an upstream accepted the request, so tokens can be used.
    generation_started: bool,
}

async fn run(state: AppState, job: Job, opened: oneshot::Sender<Result<(), Failure>>, tx: mpsc::Sender<Msg>) {
    let started = Instant::now();
    let mut opened = Some(opened);
    let mut outcome = Outcome {
        status: 200,
        ..Outcome::default()
    };
    let body = codex::backend_body(&job.body, &job.route.upstream_model, &job.cache_key);

    let work = attempts(&state, &job, &body, &mut opened, &tx, &mut outcome, started);
    let result = match tokio::time::timeout(TOTAL_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(Failure::new(
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            "The request took longer than one hour.",
        )),
    };
    if let Err(failure) = result {
        outcome.status = failure.status.as_u16();
        outcome.error_kind = Some(failure.code.to_owned());
        match opened.take() {
            Some(opened) => {
                let _ = opened.send(Err(failure));
            }
            None => {
                let _ = tx.send(Msg::Failed(failure)).await;
            }
        }
    }
    record_usage(&state, &job, &outcome, started).await;
    drop(job.permits);
}

/// Tries the accounts in order until one answers, then streams its events.
async fn attempts(
    state: &AppState,
    job: &Job,
    body: &Value,
    opened: &mut Option<oneshot::Sender<Result<(), Failure>>>,
    tx: &mpsc::Sender<Msg>,
    outcome: &mut Outcome,
    started: Instant,
) -> Result<(), Failure> {
    let accounts = match select::accounts_for(state, job.route.model_id).await {
        Ok(Selection::Accounts(accounts)) => accounts,
        Ok(Selection::NoAccount) => {
            return Err(Failure::new(
                StatusCode::NOT_FOUND,
                "model_not_found",
                format!("No linked ChatGPT account can use `{}`.", job.route.requested),
            ));
        }
        Ok(Selection::Unavailable(message)) => {
            return Err(Failure::new(StatusCode::SERVICE_UNAVAILABLE, "no_account", message));
        }
        Err(err) => return Err(db_failure(&err)),
    };

    let mut last_failure = None;
    for account in accounts {
        if tx.is_closed() {
            outcome.status = 499;
            outcome.error_kind = Some("client_closed".into());
            return Ok(());
        }
        outcome.attempts += 1;
        outcome.account = Some(account);
        if !fresh_token(state, account).await {
            last_failure = Some(Failure::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "account_unavailable",
                "The token of the ChatGPT account cannot be renewed now.",
            ));
            continue;
        }
        let response = match send_with_refresh(state, account, body).await {
            Ok(response) => response,
            Err(SendError::UsageLimit { until, message }) => {
                select::mark_limited(state, account, until).await;
                let mut failure = Failure::new(StatusCode::TOO_MANY_REQUESTS, "usage_limit_reached", message);
                failure.retry_after = Some((until - now()).max(1) as u64);
                last_failure = Some(failure);
                continue;
            }
            Err(SendError::Unauthorized) => {
                last_failure = Some(unauthorized());
                continue;
            }
            Err(SendError::Failed { status, message }) => {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                return Err(Failure::new(status, "upstream_error", message));
            }
        };
        outcome.generation_started = true;
        // The upstream accepted the request: the client gets a success status now.
        if let Some(opened) = opened.take() {
            let _ = opened.send(Ok(()));
        }
        match stream_events(state, account, response, tx, outcome, started).await {
            StreamEnd::Done => return Ok(()),
            StreamEnd::ClientGone => {
                outcome.status = 499;
                outcome.error_kind = Some("client_closed".into());
                return Ok(());
            }
            StreamEnd::Retry(failure) => {
                last_failure = Some(failure);
                continue;
            }
            StreamEnd::Failed(failure) => return Err(failure),
        }
    }
    Err(last_failure.unwrap_or_else(|| {
        Failure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_account",
            "No ChatGPT account is available.",
        )
    }))
}

fn unauthorized() -> Failure {
    Failure::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "account_unauthorized",
        "The ChatGPT account did not accept its token. Link it again.",
    )
}

/// Renews the access token when it expires soon. False when that failed.
async fn fresh_token(state: &AppState, account: i64) -> bool {
    let expires: Option<Option<i64>> =
        sqlx::query_scalar("SELECT access_expires_at FROM chatgpt_accounts WHERE id = ?")
            .bind(account)
            .fetch_optional(&state.db)
            .await
            .ok();
    let expires_soon = expires.flatten().is_none_or(|at| at - now() < REFRESH_MARGIN);
    !expires_soon || refresh::refresh(state, account, Trigger::Request).await.is_ok()
}

/// On a 401, the token is renewed once and the request sent again.
async fn send_with_refresh(state: &AppState, account: i64, body: &Value) -> Result<reqwest::Response, SendError> {
    match codex::send(state, account, body).await {
        Err(SendError::Unauthorized) => {
            if refresh::refresh(state, account, Trigger::Request).await.is_err() {
                return Err(SendError::Unauthorized);
            }
            codex::send(state, account, body).await
        }
        other => other,
    }
}

enum StreamEnd {
    Done,
    ClientGone,
    /// Failed before any content reached the client: try the next account.
    Retry(Failure),
    Failed(Failure),
}

/// Events that carry output or end the response. Until one arrives, the attempt can still move
/// to another account.
fn is_content(kind: &str) -> bool {
    kind.ends_with(".delta") || kind == "response.output_item.done" || is_terminal(kind)
}

fn is_terminal(kind: &str) -> bool {
    kind == "response.completed" || kind == "response.incomplete"
}

async fn stream_events(
    state: &AppState,
    account: i64,
    response: reqwest::Response,
    tx: &mpsc::Sender<Msg>,
    outcome: &mut Outcome,
    started: Instant,
) -> StreamEnd {
    use eventsource_stream::Eventsource;

    // Stops the stream when the upstream sends more than the byte limit.
    let mut received = 0usize;
    let bytes = response.bytes_stream().map(move |chunk| {
        let chunk = chunk?;
        received += chunk.len();
        if received > MAX_UPSTREAM_BYTES {
            return Err(TooLarge.into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(chunk)
    });
    let mut events = bytes.eventsource();
    let mut held: Vec<Event> = Vec::new();
    let mut held_bytes = 0usize;
    let mut committed = false;
    let mut output_items: Vec<Value> = Vec::new();

    loop {
        let next = tokio::select! {
            () = tx.closed() => return StreamEnd::ClientGone,
            next = tokio::time::timeout(IDLE_TIMEOUT, events.next()) => next,
        };
        let next = match next {
            Err(_) => {
                return StreamEnd::Failed(Failure::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "The ChatGPT backend sent nothing for 5 minutes.",
                ));
            }
            Ok(None) => {
                return StreamEnd::Failed(Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream_closed",
                    "The ChatGPT backend closed the stream before the end.",
                ));
            }
            Ok(Some(Err(err))) => {
                return StreamEnd::Failed(Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream_stream",
                    format!("The ChatGPT stream broke: {err}"),
                ));
            }
            Ok(Some(Ok(event))) => event,
        };
        let Ok(mut data) = serde_json::from_str::<Value>(&next.data) else {
            continue;
        };
        let kind = data["type"].as_str().unwrap_or(&next.event).to_owned();

        if kind == "response.failed" || kind == "error" {
            let error = if data["response"]["error"].is_object() {
                data["response"]["error"].clone()
            } else if data["error"].is_object() {
                data["error"].clone()
            } else {
                data.clone()
            };
            let failure = match codex::classify(400, &json!({ "error": error }).to_string(), None) {
                SendError::UsageLimit { until, message } if !committed => {
                    select::mark_limited(state, account, until).await;
                    return StreamEnd::Retry(Failure::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "usage_limit_reached",
                        message,
                    ));
                }
                SendError::UsageLimit { message, .. } => {
                    Failure::new(StatusCode::TOO_MANY_REQUESTS, "usage_limit_reached", message)
                }
                SendError::Unauthorized if !committed => {
                    let _ = refresh::refresh(state, account, Trigger::Request).await;
                    return StreamEnd::Retry(unauthorized());
                }
                SendError::Unauthorized => unauthorized(),
                SendError::Failed { message, .. } => Failure::new(StatusCode::BAD_GATEWAY, "upstream_error", message),
            };
            return StreamEnd::Failed(failure);
        }

        if kind == "response.output_item.done" {
            output_items.push(data["item"].clone());
        }
        if kind.ends_with(".delta") {
            if outcome.first_token_ms.is_none() {
                outcome.first_token_ms = Some(started.elapsed().as_millis() as i64);
            }
            outcome.streamed_chars += data["delta"].as_str().map_or(0, str::len);
        }
        let completed = is_terminal(&kind);
        if completed {
            // The backend may send an empty output list at the end; the items came one by one.
            let empty = data["response"]["output"].as_array().is_none_or(Vec::is_empty);
            if empty && !output_items.is_empty() {
                data["response"]["output"] = Value::Array(std::mem::take(&mut output_items));
            }
            outcome.final_response = Some(data["response"].clone());
        }

        let event = Event { kind, data };
        if committed {
            if tx.send(Msg::Event(event)).await.is_err() {
                return StreamEnd::ClientGone;
            }
        } else {
            held_bytes += next.data.len();
            let commit = is_content(&event.kind) || held.len() >= MAX_HELD_EVENTS || held_bytes >= MAX_HELD_BYTES;
            held.push(event);
            if commit {
                committed = true;
                for event in held.drain(..) {
                    if tx.send(Msg::Event(event)).await.is_err() {
                        return StreamEnd::ClientGone;
                    }
                }
            }
        }

        if completed {
            let response = outcome.final_response.clone().unwrap_or_default();
            if tx.send(Msg::Done(response)).await.is_err() {
                return StreamEnd::ClientGone;
            }
            return StreamEnd::Done;
        }
    }
}

#[derive(Debug)]
struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the answer is larger than 64 MB")
    }
}

impl std::error::Error for TooLarge {}

async fn record_usage(state: &AppState, job: &Job, outcome: &Outcome, started: Instant) {
    let response = outcome.final_response.as_ref();
    let usage = response.map(|r| &r["usage"]).filter(|u| u.is_object());
    let (tokens, usage_status) = match usage {
        Some(usage) => (Tokens::from_openai(usage), "reported"),
        None if outcome.generation_started => (
            Tokens {
                input_text: crate::tokens::estimate(&job.body).await as i64,
                output_text: (outcome.streamed_chars / 4) as i64,
                ..Tokens::default()
            },
            "estimated",
        ),
        None => (Tokens::default(), "none"),
    };
    // Without reported usage the real use is unknown, so keep at least the reservation.
    let used = match usage_status {
        "estimated" => (tokens.input() + tokens.output()).max(job.reserved_tokens),
        _ => tokens.input() + tokens.output(),
    };
    state.key_limits.settle_tokens(job.key.id, job.reserved_tokens, used);

    let web_search_calls = response.and_then(|r| r["output"].as_array()).map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["type"] == "web_search_call" && item["action"]["type"] == "search")
            .count() as i64
    });
    let row = Row {
        request_id: random_token(12),
        time: now(),
        api_key_id: job.key.id,
        route: job.route_name,
        client_format: job.client_format,
        upstream: "chatgpt",
        chatgpt_account_id: outcome.account,
        requested_model: job.route.requested.clone(),
        resolved_model: Some(job.route.qualified.clone()),
        effort: job.body["reasoning"]["effort"].as_str().map(str::to_owned),
        service_tier_requested: job.body["service_tier"].as_str().map(str::to_owned),
        service_tier_reported: response.and_then(|r| r["service_tier"].as_str()).map(str::to_owned),
        streamed: job.stream,
        status_code: outcome.status,
        error_kind: outcome.error_kind.clone(),
        latency_ms: started.elapsed().as_millis() as i64,
        first_token_ms: outcome.first_token_ms,
        usage_status,
        tokens,
        web_search_calls,
        failover_attempts: (outcome.attempts - 1).max(0),
    };
    state.usage.record(row).await;
}

fn db_failure(err: &sqlx::Error) -> Failure {
    tracing::error!(error = %err, "database error in the gateway");
    Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "Internal error.")
}
