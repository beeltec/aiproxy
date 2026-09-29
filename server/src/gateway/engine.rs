//! Runs one gateway request against the ChatGPT backend. The internal format is the OpenAI
//! Responses API: requests arrive as a Responses body, and the stream is Responses events.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::auth::{Admission, ApiKey};
use super::codex::{self, SendError};
use super::provider;
use super::routing::{Route, Upstream};
use crate::chatgpt::refresh::{self, Trigger};
use crate::chatgpt::select::{self, Selection};
use crate::crypto::random_token;
use crate::db::now;
use crate::state::AppState;
use crate::usage::{Extras, Media, Row, Step, Tokens};

/// Longest time without any upstream event.
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest time for one request.
pub(super) const TOTAL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Events held before the attempt commits (then it commits anyway).
const MAX_HELD_EVENTS: usize = 1000;
const MAX_HELD_BYTES: usize = 1024 * 1024;
/// Upper bound for everything one upstream answer may send.
pub(super) const MAX_UPSTREAM_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound for the output items kept for the final response.
pub(super) const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
/// Longest wait to give a late error to a client that does not read.
const ERROR_SEND_TIMEOUT: Duration = Duration::from_secs(5);
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
    /// Native pass-through: one SSE frame as the upstream sent it.
    Raw(bytes::Bytes),
    /// Native pass-through without streaming: the upstream answer body.
    Native(Value),
}

pub struct Job {
    pub key: ApiKey,
    pub route: Route,
    /// Responses request body from the client (before the backend adjustments).
    pub body: Value,
    /// The body as the client sent it, for native pass-through.
    pub native: Value,
    /// The `anthropic-beta` header of an Anthropic client.
    pub anthropic_beta: Option<String>,
    /// The `anthropic-version` header of an Anthropic client.
    pub anthropic_version: Option<String>,
    pub route_name: &'static str,
    pub client_format: &'static str,
    pub stream: bool,
    pub cache_key: String,
    /// Tokens reserved for the tokens-per-minute limit.
    pub reserved_tokens: i64,
    /// Concurrency slots. They stay taken until the engine and the response body end.
    pub _permits: Admission,
}

/// Starts the request. Returns when the upstream accepted it, or with the error to send as
/// the HTTP answer. The work runs in its own task. A closed client stops the upstream request,
/// and the usage is still recorded.
pub async fn start(state: &AppState, job: Job) -> Result<mpsc::Receiver<Msg>, Failure> {
    let (opened_tx, opened_rx) = oneshot::channel();
    let (tx, rx) = mpsc::channel(64);
    let state = state.clone();
    state.gateway_tasks.clone().spawn(run(state, job, opened_tx, tx));
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
pub(super) struct Outcome {
    pub status: u16,
    pub error_kind: Option<String>,
    pub account: Option<i64>,
    pub final_response: Option<Value>,
    /// Usage in billing categories, when the upstream format is not OpenAI's.
    pub tokens: Option<Tokens>,
    /// The usage facts besides the tokens, with `tokens`.
    pub extras: Extras,
    /// The sampling steps of `tokens` (Anthropic answers and their iterations).
    pub steps: Vec<Step>,
    /// The service tier that a native Chat or Messages answer reported.
    pub service_tier: Option<String>,
    pub first_token_ms: Option<i64>,
    pub streamed_chars: usize,
    pub attempts: i64,
    /// Completed web searches, also known when the stream stops early.
    pub web_search_calls: i64,
    /// True after an upstream accepted the request, so tokens can be used.
    pub generation_started: bool,
    /// Web searches may have run, but the upstream does not report their number.
    pub searches_uncounted: bool,
    /// The upstream format of a native stream: its errors go out in that format.
    pub native_wire: Option<provider::Wire>,
    /// Finished image-generation calls without their image data, also known when the stream
    /// stops early.
    pub image_calls: Vec<Value>,
}

impl Outcome {
    /// Keeps a finished output item when it is an image-generation call.
    pub fn note_item(&mut self, item: &Value) {
        if item["type"] != "image_generation_call" {
            return;
        }
        let mut call = item.clone();
        if call["result"].is_string() {
            call["result"] = json!("");
        }
        self.image_calls.push(call);
    }
}

async fn run(state: AppState, job: Job, opened: oneshot::Sender<Result<(), Failure>>, tx: mpsc::Sender<Msg>) {
    let started = Instant::now();
    let mut opened = Some(opened);
    let mut outcome = Outcome {
        status: 200,
        ..Outcome::default()
    };
    let work = async {
        match job.route.upstream {
            Upstream::ChatGpt => {
                let body = codex::backend_body(&job.body, &job.route.upstream_model, &job.cache_key);
                attempts(&state, &job, &body, &mut opened, &tx, &mut outcome, started).await
            }
            Upstream::Connection { id, kind } => {
                provider::attempt(&state, &job, id, kind, &mut opened, &tx, &mut outcome, started).await
            }
        }
    };

    // Dropping the work future also stops the upstream request.
    let result = tokio::select! {
        result = tokio::time::timeout(TOTAL_TIMEOUT, work) => result.unwrap_or_else(|_| {
            Err(Failure::new(StatusCode::GATEWAY_TIMEOUT, "timeout", "The request took longer than one hour."))
        }),
        () = tx.closed() => Err(client_closed()),
        () = state.stopping.cancelled() => Err(Failure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "The server is stopping.",
        )),
    };
    if let Err(failure) = result {
        outcome.status = if failure.code == CLIENT_CLOSED {
            499
        } else {
            failure.status.as_u16()
        };
        outcome.error_kind = Some(failure.code.to_owned());
        match opened.take() {
            Some(opened) => {
                let _ = opened.send(Err(failure));
            }
            None => {
                let send = async {
                    match outcome.native_wire {
                        Some(wire) => {
                            for frame in provider::native_error(wire, &failure) {
                                let _ = tx.send(Msg::Raw(frame)).await;
                            }
                        }
                        None => {
                            let _ = tx.send(Msg::Failed(failure)).await;
                        }
                    }
                };
                let _ = tokio::time::timeout(ERROR_SEND_TIMEOUT, send).await;
            }
        }
    }
    record_usage(&state, &job, &outcome, started).await;
}

pub(super) const CLIENT_CLOSED: &str = "client_closed";

pub(super) fn client_closed() -> Failure {
    Failure::new(
        StatusCode::BAD_REQUEST,
        CLIENT_CLOSED,
        "The client closed the connection.",
    )
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
    'accounts: for account in accounts {
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
        // A token error in the stream renews the token and sends the request to the same
        // account once more.
        let mut renewed = false;
        loop {
            let response = match send_with_refresh(state, account, body).await {
                Ok(response) => response,
                Err(error) => match failure_of(state, account, error).await {
                    Next::Account(failure) => {
                        last_failure = Some(failure);
                        continue 'accounts;
                    }
                    Next::Stop(failure) => return Err(failure),
                },
            };
            outcome.generation_started = true;
            // The upstream accepted the request: the client gets a success status now.
            if let Some(opened) = opened.take() {
                let _ = opened.send(Ok(()));
            }
            match stream_events(state, account, response, tx, outcome, started).await {
                StreamEnd::Done => return Ok(()),
                StreamEnd::ClientGone => return Err(client_closed()),
                StreamEnd::Unauthorized if !renewed => {
                    if refresh::refresh(state, account, Trigger::Request).await.is_err() {
                        last_failure = Some(unauthorized());
                        continue 'accounts;
                    }
                    renewed = true;
                }
                StreamEnd::Unauthorized => {
                    last_failure = Some(unauthorized());
                    continue 'accounts;
                }
                StreamEnd::Retry(failure) => {
                    last_failure = Some(failure);
                    continue 'accounts;
                }
                StreamEnd::Failed(failure) => return Err(failure),
            }
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

enum Next {
    /// Try the next account.
    Account(Failure),
    Stop(Failure),
}

/// What an upstream error means for the account loop.
async fn failure_of(state: &AppState, account: i64, error: SendError) -> Next {
    match error {
        SendError::UsageLimit { until, message } => {
            let until = select::mark_limited(state, account, until).await;
            let mut failure = Failure::new(StatusCode::TOO_MANY_REQUESTS, "usage_limit_reached", message);
            failure.retry_after = Some((until - now()).max(1) as u64);
            Next::Account(failure)
        }
        SendError::Throttled { retry_after, message } => {
            let mut failure = Failure::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded", message);
            failure.retry_after = retry_after.map(|s| s.max(1) as u64);
            Next::Account(failure)
        }
        SendError::Unauthorized => Next::Account(unauthorized()),
        SendError::Failed { status, message } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            Next::Stop(Failure::new(status, "upstream_error", message))
        }
    }
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
    /// The token was refused before any content reached the client.
    Unauthorized,
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
    let mut events = limited_events(response);
    let mut held: Vec<Event> = Vec::new();
    let mut held_bytes = 0usize;
    let mut committed = false;
    let mut output_items: Vec<Value> = Vec::new();
    let mut output_bytes = 0usize;

    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, events.next()).await {
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
        if next.data.len() > MAX_EVENT_BYTES {
            return StreamEnd::Failed(too_large("The ChatGPT backend sent an event larger than 16 MB."));
        }
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
            // Only an invalid request is the fault of the client; other failures are upstream errors.
            let invalid = error["type"] == "invalid_request_error"
                || error["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("invalid") || code == "context_length_exceeded");
            let status = if invalid { 400 } else { 502 };
            let error = codex::classify(status, &json!({ "error": error }).to_string(), None);
            return match error {
                SendError::Unauthorized if !committed => StreamEnd::Unauthorized,
                SendError::Unauthorized => StreamEnd::Failed(unauthorized()),
                error => match failure_of(state, account, error).await {
                    Next::Account(failure) if !committed => StreamEnd::Retry(failure),
                    Next::Account(failure) | Next::Stop(failure) => StreamEnd::Failed(failure),
                },
            };
        }

        if kind == "response.output_item.done" {
            output_bytes += next.data.len();
            if output_bytes > MAX_OUTPUT_BYTES {
                return StreamEnd::Failed(too_large("The answer is larger than 32 MB."));
            }
            if data["item"]["type"] == "web_search_call" && data["item"]["action"]["type"] == "search" {
                outcome.web_search_calls += 1;
            }
            outcome.note_item(&data["item"]);
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

/// The SSE events of an upstream answer. The stream fails when the upstream sends more than
/// the byte limit.
pub(super) fn limited_events(
    response: reqwest::Response,
) -> impl futures_util::Stream<
    Item = Result<
        eventsource_stream::Event,
        eventsource_stream::EventStreamError<Box<dyn std::error::Error + Send + Sync>>,
    >,
> + Unpin {
    use eventsource_stream::Eventsource;
    let mut received = 0usize;
    let bytes = response.bytes_stream().map(move |chunk| {
        let chunk = chunk?;
        received += chunk.len();
        if received > MAX_UPSTREAM_BYTES {
            return Err(TooLarge.into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(chunk)
    });
    bytes.eventsource()
}

pub(super) fn too_large(message: &str) -> Failure {
    Failure::new(StatusCode::BAD_GATEWAY, "upstream_too_large", message)
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
    let (tokens, usage_status) = match (&outcome.tokens, usage) {
        (Some(tokens), _) => (tokens.clone(), "reported"),
        (None, Some(usage)) => (Tokens::from_openai(usage), "reported"),
        (None, None) if outcome.generation_started => (
            Tokens {
                input_text: crate::tokens::estimate(&job.body).await as i64,
                output_text: (outcome.streamed_chars / 4) as i64,
                ..Tokens::default()
            },
            "estimated",
        ),
        (None, None) => (Tokens::default(), "none"),
    };
    let mut tokens = tokens;
    tokens.inexact |= outcome.searches_uncounted;
    // A stream that stopped early has no final response, but its finished image calls count.
    let image_tool = response.and_then(image_tool_usage).or_else(|| {
        let calls = json!({ "output": outcome.image_calls, "tools": job.body["tools"] });
        image_tool_usage(&calls)
    });
    // The Images API handler answers a response without an image with an error.
    let no_image = job.client_format == "images"
        && outcome.status == 200
        && image_tool
            .as_ref()
            .is_none_or(|(_, _, _, media)| media.images_generated == 0);
    let (status_code, error_kind) = if no_image {
        (StatusCode::BAD_GATEWAY.as_u16(), Some("no_image".to_owned()))
    } else {
        (outcome.status, outcome.error_kind.clone())
    };
    let image_estimated = image_tool
        .as_ref()
        .is_some_and(|(_, _, status, _)| *status == "estimated");
    let image_tokens = image_tool
        .as_ref()
        .map_or(0, |(_, tokens, _, _)| tokens.input() + tokens.output());
    let used = tokens.input() + tokens.output() + image_tokens;
    // Without reported usage the real use is unknown, so keep at least the reservation.
    let used = if usage_status == "estimated" || image_estimated {
        used.max(job.reserved_tokens)
    } else {
        used
    };
    state.key_limits.settle_tokens(job.key.id, job.reserved_tokens, used);

    // A Responses final answer has the facts in its usage.
    let extras = match (&outcome.tokens, response) {
        (None, Some(response)) => Extras::from_usage(&response["usage"]),
        _ => outcome.extras.clone(),
    };
    // An answer with several sampling steps (Anthropic continuations and iterations) has one
    // row per step; the first step is on the model row.
    let mut steps = match &outcome.tokens {
        Some(_) if outcome.steps.len() > 1 => outcome.steps.clone(),
        _ => Vec::new(),
    };
    if let Some(first) = steps.first_mut() {
        first.tokens.inexact |= outcome.searches_uncounted;
    }
    // Each step keeps its own facts; the reported cost is for the whole request.
    let service_tier_reported = outcome
        .service_tier
        .clone()
        .or_else(|| response.and_then(|r| r["service_tier"].as_str()).map(str::to_owned));
    let (row_tokens, row_status, row_extras, row_tier) = match steps.first() {
        Some(first) => (
            first.tokens.clone(),
            if first.estimated { "estimated" } else { usage_status },
            Extras {
                reported_cost_nano: extras.reported_cost_nano,
                ..first.extras.clone()
            },
            // A step keeps its own tier, also when it reported none.
            first.service_tier.clone(),
        ),
        None => (tokens, usage_status, extras, service_tier_reported),
    };
    // Prices differ per search tool; a request has one of them.
    let tools = job.body["tools"].as_array().map(Vec::as_slice).unwrap_or_default();
    let preview_search = tools.iter().any(|tool| tool["type"] == "web_search_preview")
        && !tools.iter().any(|tool| tool["type"] == "web_search");
    let row = Row {
        request_id: random_token(12),
        component: "model",
        time: now(),
        api_key_id: job.key.id,
        route: job.route_name,
        client_format: job.client_format,
        upstream: match job.route.upstream {
            Upstream::ChatGpt => "chatgpt",
            Upstream::Connection { kind, .. } => kind.as_str(),
        },
        chatgpt_account_id: outcome.account,
        connection_id: match job.route.upstream {
            Upstream::ChatGpt => None,
            Upstream::Connection { id, .. } => Some(id),
        },
        requested_model: job.route.requested.clone(),
        alias: job.route.alias.as_ref().map(|alias| alias.name.clone()),
        resolved_model: Some(job.route.qualified.clone()),
        effort: job.body["reasoning"]["effort"].as_str().map(str::to_owned),
        service_tier_requested: job.body["service_tier"].as_str().map(str::to_owned),
        service_tier_reported: row_tier,
        streamed: job.stream,
        status_code,
        error_kind,
        latency_ms: started.elapsed().as_millis() as i64,
        first_token_ms: outcome.first_token_ms,
        usage_status: row_status,
        tokens: row_tokens,
        web_search_calls: if preview_search { 0 } else { outcome.web_search_calls },
        web_search_preview_calls: if preview_search { outcome.web_search_calls } else { 0 },
        failover_attempts: (outcome.attempts - 1).max(0),
        extras: row_extras,
        media: Media {
            input_images: input_images(&job.body["input"]),
            ..Media::default()
        },
    };
    // The image tool is its own billing component: the image model with its own tokens.
    let image_row = image_tool.map(|(model, tokens, usage_status, media)| Row {
        component: "image_tool",
        resolved_model: Some(model),
        effort: None,
        service_tier_requested: None,
        service_tier_reported: None,
        first_token_ms: None,
        usage_status,
        tokens,
        web_search_calls: 0,
        web_search_preview_calls: 0,
        failover_attempts: 0,
        media,
        extras: Extras::default(),
        ..row.clone()
    });
    let step_rows: Vec<Row> = steps
        .into_iter()
        .skip(1)
        .map(|step| Row {
            component: "iteration",
            first_token_ms: None,
            usage_status: if step.estimated { "estimated" } else { usage_status },
            service_tier_reported: step.service_tier,
            tokens: step.tokens,
            web_search_calls: 0,
            web_search_preview_calls: 0,
            media: Media::default(),
            extras: step.extras,
            ..row.clone()
        })
        .collect();
    let mut rows = vec![row];
    rows.extend(step_rows);
    rows.extend(image_row);
    state.usage.record_all(rows).await;
}

/// Images in the request input (content parts and tool outputs).
fn input_images(input: &Value) -> i64 {
    let parts = |item: &Value| {
        ["content", "output"]
            .iter()
            .flat_map(|field| item[*field].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "input_image")
            .count()
    };
    input.as_array().into_iter().flatten().map(parts).sum::<usize>() as i64
}

/// The image-generation tool component of a response: the image model, its tokens, the usage
/// status and the images. `None` when the response has no image-generation call.
fn image_tool_usage(response: &Value) -> Option<(String, Tokens, &'static str, Media)> {
    let calls: Vec<&Value> = response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "image_generation_call")
        .collect();
    let first = calls.first()?;
    let usage = &response["tool_usage"]["image_gen"];
    let (tokens, usage_status) = if usage.is_object() {
        (Tokens::from_image(usage, false), "reported")
    } else {
        (Tokens::default(), "estimated")
    };
    let model = response["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["type"] == "image_generation")
        .and_then(|tool| tool["model"].as_str())
        .unwrap_or("image_generation")
        .to_owned();
    let media = Media {
        images_generated: calls.iter().filter(|call| call["result"].is_string()).count() as i64,
        image_size: first["size"].as_str().map(str::to_owned),
        image_quality: first["quality"].as_str().map(str::to_owned),
        ..Media::default()
    };
    Some((model, tokens, usage_status, media))
}

fn db_failure(err: &sqlx::Error) -> Failure {
    tracing::error!(error = %err, "database error in the gateway");
    Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "Internal error.")
}
