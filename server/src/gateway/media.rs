//! Decisions, embeddings, audio and images. These go natively to OpenAI or OpenRouter connections; images
//! for ChatGPT models run through the Responses image-generation tool. Each call runs in its own
//! task, so a client that leaves does not lose the usage row.

use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::auth::{Admission, ApiKey};
use super::engine::{self, Failure, IDLE_TIMEOUT, MAX_EVENT_BYTES, MAX_OUTPUT_BYTES, Msg, TOTAL_TIMEOUT};
use super::responses_api::error;
use super::routing::{Route, Upstream};
use super::{provider, request, sse};
use crate::connections::{Connection, Kind};
use crate::crypto::random_token;
use crate::db::now;
use crate::state::AppState;
use crate::usage::{Extras, Media, Row, Tokens};

/// A request that the gateway accepted, with what the usage row needs.
#[derive(Clone)]
struct Call {
    key: ApiKey,
    route: Route,
    connection_id: i64,
    kind: Kind,
    route_name: &'static str,
    streamed: bool,
    reserved: i64,
    /// The usage known before the answer, for a row when the answer has none.
    estimate: Tokens,
    media: Media,
    inference_geo: Option<&'static str>,
    started: Instant,
    _permits: Admission,
}

impl Call {
    /// The end of the total time of the request.
    fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.started + TOTAL_TIMEOUT)
    }
}

/// The usage of a finished call.
struct Recorded {
    status: u16,
    error_kind: Option<String>,
    usage_status: &'static str,
    tokens: Tokens,
    media: Media,
    /// The answer has no usage: the estimate of the call replaces the tokens and media.
    unknown: bool,
    /// The cost that the provider reported.
    extras: Extras,
}

impl Recorded {
    fn ok(usage_status: &'static str, tokens: Tokens, media: Media) -> Self {
        Self {
            status: 200,
            error_kind: None,
            usage_status,
            tokens,
            media,
            unknown: false,
            extras: Extras::default(),
        }
    }

    /// A successful answer without usage.
    fn unknown() -> Self {
        Self {
            unknown: true,
            ..Self::ok("estimated", Tokens::default(), Media::default())
        }
    }

    /// A failure before the upstream did work: nothing was used.
    fn failed(failure: &Failure) -> Self {
        Self {
            status: failure.status.as_u16(),
            error_kind: Some(failure.code.to_owned()),
            usage_status: "none",
            tokens: Tokens::default(),
            media: Media::default(),
            unknown: false,
            extras: Extras::default(),
        }
    }

    /// A failure after the upstream accepted the work: the use is unknown.
    fn lost(failure: &Failure) -> Self {
        Self {
            usage_status: "estimated",
            unknown: true,
            ..Self::failed(failure)
        }
    }
}

/// The answer of a call and its row. A stream writes its row later.
type Done = (Response, Option<Recorded>);

/// The answer and the row of a failure before the upstream did work.
fn failed(failure: Failure) -> Done {
    let recorded = Recorded::failed(&failure);
    (error(failure), Some(recorded))
}

/// The answer and the row of a failure after the upstream accepted the work.
fn lost(failure: Failure) -> Done {
    let recorded = Recorded::lost(&failure);
    (error(failure), Some(recorded))
}

fn bad(message: impl Into<String>) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn timed_out() -> Failure {
    Failure::new(
        StatusCode::GATEWAY_TIMEOUT,
        "timeout",
        "The request took longer than one hour.",
    )
}

fn shutting_down() -> Failure {
    Failure::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "shutting_down",
        "The server is stopping.",
    )
}

async fn record(state: &AppState, call: &Call, mut recorded: Recorded) {
    if recorded.unknown {
        recorded.tokens = call.estimate.clone();
        recorded.media = call.media.clone();
    }
    if let Some(geo) = call.inference_geo {
        recorded.extras.inference_geo = Some(geo.to_owned());
    }
    let used = match recorded.tokens.input() + recorded.tokens.output() {
        // Duration-priced audio reports no tokens; its length still counts for the limit.
        0 => audio_estimate(recorded.media.seconds, 0),
        used => used,
    };
    // Without reported usage the real use is unknown, so keep at least the reservation.
    let used = if recorded.usage_status == "estimated" {
        used.max(call.reserved)
    } else {
        used
    };
    state.key_limits.settle_tokens(call.key.id, call.reserved, used);
    let row = Row {
        request_id: random_token(12),
        component: "model",
        time: now(),
        api_key_id: call.key.id,
        route: call.route_name,
        client_format: call.route_name,
        upstream: call.kind.as_str(),
        chatgpt_account_id: None,
        connection_id: Some(call.connection_id),
        requested_model: call.route.requested.clone(),
        alias: call.route.alias.as_ref().map(|alias| alias.name.clone()),
        resolved_model: Some(call.route.qualified.clone()),
        effort: None,
        service_tier_requested: None,
        service_tier_reported: None,
        streamed: call.streamed,
        status_code: recorded.status,
        error_kind: recorded.error_kind,
        latency_ms: call.started.elapsed().as_millis() as i64,
        first_token_ms: None,
        usage_status: recorded.usage_status,
        tokens: recorded.tokens,
        web_search_calls: 0,
        web_search_preview_calls: 0,
        failover_attempts: 0,
        media: recorded.media,
        extras: recorded.extras,
    };
    state.usage.record(row).await;
}

/// Runs the work in its own task. The work returns the response and its usage, or no usage
/// when a stream records it later. A timeout, a client that leaves, or a shutdown stops the
/// work and records a row.
async fn detached<F>(state: &AppState, call: Call, work: F) -> Response
where
    F: Future<Output = Done> + Send + 'static,
{
    let (mut tx, rx) = oneshot::channel();
    let task_state = state.clone();
    state.gateway_tasks.clone().spawn(async move {
        let result = tokio::select! {
            result = tokio::time::timeout_at(call.deadline(), work) => result.map_err(|_| timed_out()),
            () = tx.closed() => Err(engine::client_closed()),
            () = task_state.stopping.cancelled() => Err(shutting_down()),
        };
        let (response, recorded) = result.unwrap_or_else(lost);
        if let Some(recorded) = recorded {
            record(&task_state, &call, recorded).await;
        }
        let _ = tx.send(response);
    });
    rx.await.unwrap_or_else(|_| {
        error(Failure::new(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "The request stopped.",
        ))
    })
}

/// The connection of a model, when its kind can serve the endpoint.
async fn target(
    state: &AppState,
    key: &ApiKey,
    body_model: &Value,
    kinds: &[Kind],
    what: &str,
) -> Result<(Route, i64, Kind), Failure> {
    let model = body_model
        .as_str()
        .ok_or_else(|| bad("The field `model` is missing."))?;
    let route = request::route(state, key, model).await?;
    match route.upstream {
        Upstream::Connection { id, kind } if kinds.contains(&kind) => Ok((route, id, kind)),
        _ => {
            let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
            Err(bad(format!(
                "{what} work only with models of {} connections.",
                names.join(" or ")
            )))
        }
    }
}

fn reserve(state: &AppState, key: &ApiKey, tokens: i64) -> Result<i64, Failure> {
    state.key_limits.reserve_tokens(key.id, tokens).map_err(|wait| {
        state.rejected.count(super::rejected::Reason::RateLimited);
        let mut failure = Failure::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            "This key used its tokens per minute.",
        );
        failure.retry_after = Some(wait);
        failure
    })
}

/// A POST request to `<base URL>/<path>` of the connection.
async fn upstream(state: &AppState, connection_id: i64, path: &str) -> Result<reqwest::RequestBuilder, Failure> {
    let connection = Connection::load(state, connection_id).await.map_err(|err| {
        tracing::error!(connection = connection_id, error = %err, "cannot load the connection");
        Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "Internal error.")
    })?;
    connection
        .request(state, reqwest::Method::POST, path, None)
        .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))
}

/// Sends the request. The upstream must send the answer headers within `headers_within`. A
/// failure gives the answer and the row: an error answer or a failed connection used nothing,
/// but after a timeout or a broken connection the upstream can have done the work.
async fn send(request: reqwest::RequestBuilder, headers_within: Duration) -> Result<reqwest::Response, Box<Done>> {
    let response = match tokio::time::timeout(headers_within, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            let failure = Failure::new(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("Cannot reach the upstream: {err}"),
            );
            return Err(Box::new(if err.is_connect() {
                failed(failure)
            } else {
                lost(failure)
            }));
        }
        Err(_) => {
            return Err(Box::new(lost(Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "The upstream did not answer in time.",
            ))));
        }
    };
    if !response.status().is_success() {
        return Err(Box::new(failed(provider::error_answer(response).await)));
    }
    Ok(response)
}

/// Without streaming, the upstream sends the headers only when the whole answer is ready.
fn headers_limit(streamed: bool) -> Duration {
    if streamed {
        provider::HEADERS_TIMEOUT
    } else {
        TOTAL_TIMEOUT
    }
}

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned()
}

fn broken(err: &reqwest::Error) -> Failure {
    Failure::new(
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        format!("The upstream answer broke: {err}"),
    )
}

fn ended_early() -> Failure {
    Failure::new(
        StatusCode::BAD_GATEWAY,
        "upstream_closed",
        "The upstream stream ended before its last event.",
    )
}

fn idle() -> Failure {
    Failure::new(
        StatusCode::GATEWAY_TIMEOUT,
        "upstream_timeout",
        "The upstream sent nothing for 5 minutes.",
    )
}

/// Reads a whole answer body with the size limit and the idle timeout.
async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout(IDLE_TIMEOUT, chunks.next())
            .await
            .map_err(|_| idle())?;
        let Some(chunk) = chunk else { break };
        body.extend_from_slice(&chunk.map_err(|err| broken(&err))?);
        if body.len() > MAX_OUTPUT_BYTES {
            return Err(engine::too_large("The answer is larger than 32 MB."));
        }
    }
    Ok(body)
}

/// Reads a whole JSON answer.
async fn read_json(response: reqwest::Response) -> Result<Value, Failure> {
    serde_json::from_slice(&read_body(response).await?).map_err(|_| {
        Failure::new(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "The upstream answer is not JSON.",
        )
    })
}

fn with_content_type(mut response: Response, content_type: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// The time that the upstream has to close the body after its last event.
const END_WAIT: Duration = Duration::from_secs(5);

/// The last events of the SSE media answers (besides the `.completed` image events).
const TERMINAL_EVENTS: [&str; 2] = ["speech.audio.done", "transcript.text.done"];

/// Forwards a streamed answer (SSE or binary audio) as it is. `finish` gets the last SSE event
/// that has a `usage` object or ends the answer (without image data) and gives the usage row,
/// which is written when the stream ends. An SSE answer without its last event, an `error`
/// event, or another failure ends the body with an error, so the client does not take a cut
/// answer as complete.
fn forward(
    state: &AppState,
    call: Call,
    response: reqwest::Response,
    finish: impl FnOnce(Option<Value>) -> Recorded + Send + 'static,
) -> Response {
    let content_type = content_type(&response);
    let sse = content_type.starts_with("text/event-stream");
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let task_state = state.clone();
    state.gateway_tasks.clone().spawn(async move {
        let deadline = call.deadline();
        let mut chunks = response.bytes_stream();
        let mut line = Vec::new();
        let mut last = None;
        let mut ended = false;
        let mut upstream_error = None;
        // A keep-alive comment may only go between two events.
        let mut between_events = true;
        let mut idle_until = tokio::time::Instant::now() + IDLE_TIMEOUT;
        let mut end_by = None;
        let failure = loop {
            let next = tokio::select! {
                next = chunks.next() => next,
                () = tokio::time::sleep(sse::PING_AFTER), if sse && between_events => {
                    // A full channel already has data for the client.
                    let _ = tx.try_send(Ok(Bytes::from_static(b": keep-alive\n\n")));
                    continue;
                }
                // After the last event only the end of the body is missing.
                () = tokio::time::sleep_until(idle_until) => break if ended { None } else { Some(idle()) },
                () = tokio::time::sleep_until(deadline) => break Some(timed_out()),
                () = tx.closed() => break Some(engine::client_closed()),
                () = task_state.stopping.cancelled() => break Some(shutting_down()),
            };
            let chunk = match next {
                Some(Ok(chunk)) => chunk,
                None if sse && !ended => {
                    break Some(upstream_error.take().unwrap_or_else(ended_early));
                }
                None => break upstream_error.take(),
                Some(Err(err)) => break Some(broken(&err)),
            };
            if sse {
                let mut pieces = chunk.split(|byte| *byte == b'\n').peekable();
                let mut too_long = false;
                while let Some(piece) = pieces.next() {
                    if line.len() + piece.len() > MAX_EVENT_BYTES {
                        too_long = true;
                        break;
                    }
                    line.extend_from_slice(piece);
                    // The last piece has no line end yet.
                    if pieces.peek().is_none() {
                        if !line.is_empty() {
                            between_events = false;
                        }
                        break;
                    }
                    // An empty line ends an event.
                    between_events = line.is_empty() || line == b"\r";
                    let data = line
                        .strip_suffix(b"\r")
                        .unwrap_or(&line)
                        .strip_prefix(b"data:")
                        .map(|data| data.strip_prefix(b" ").unwrap_or(data));
                    if data == Some(b"[DONE]") {
                        ended = true;
                    }
                    if let Some(data) = data
                        && let Ok(mut event) = serde_json::from_slice::<Value>(data)
                    {
                        let kind = event["type"].as_str().unwrap_or_default();
                        ended |= TERMINAL_EVENTS.contains(&kind) || kind.ends_with(".completed");
                        if event["type"] == "error" {
                            let message = event["error"]["message"].as_str().or(event["message"].as_str());
                            upstream_error = Some(Failure::new(
                                StatusCode::BAD_GATEWAY,
                                "upstream_error",
                                message.unwrap_or("The upstream failed.").to_owned(),
                            ));
                        }
                        if event["usage"].is_object() || ended {
                            if let Some(event) = event.as_object_mut() {
                                event.remove("b64_json");
                            }
                            last = Some(event);
                        }
                    }
                    line.clear();
                }
                if too_long {
                    break Some(engine::too_large("An event of the answer is larger than 16 MB."));
                }
            }
            // The end wait starts once, at the last event.
            if ended {
                idle_until = *end_by.get_or_insert_with(|| tokio::time::Instant::now() + END_WAIT);
            } else {
                idle_until = tokio::time::Instant::now() + IDLE_TIMEOUT;
            }
            tokio::select! {
                sent = tx.send(Ok(chunk)) => if sent.is_err() {
                    break Some(engine::client_closed());
                },
                () = tokio::time::sleep_until(deadline) => break Some(timed_out()),
                () = task_state.stopping.cancelled() => break Some(shutting_down()),
            }
        };
        let mut recorded = finish(last);
        if let Some(failure) = failure {
            if failure.code != engine::CLIENT_CLOSED {
                let message = std::io::Error::other(failure.message.clone());
                let _ = tokio::time::timeout(Duration::from_secs(5), tx.send(Err(message))).await;
            }
            recorded.status = failure.status.as_u16();
            recorded.error_kind = Some(failure.code.to_owned());
        }
        record(&task_state, &call, recorded).await;
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut response = with_content_type(Body::from_stream(stream).into_response(), &content_type);
    if sse {
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    }
    response
}

fn json_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, Failure> {
    match body {
        Ok(Json(body)) if body.is_object() => Ok(body),
        Ok(_) => Err(bad("The body must be a JSON object.")),
        Err(rejection) => Err(request::bad_json(&rejection)),
    }
}

// ---------------------------------------------------------------------------------------------
// Decisions

pub async fn decisions(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let result = async {
        let mut body = json_body(body)?;
        let (route, connection_id, kind) = target(&state, &key, &body["model"], &[Kind::OpenAi], "Decisions").await?;
        if route.upstream_model != "gpt-6-luna" {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "unsupported_model",
                "Decisions supports only gpt-6-luna on OpenAI API-key connections.",
            ));
        }
        if body.get("stream").is_some() {
            return Err(bad("Decisions does not support the `stream` field."));
        }
        body["model"] = json!(route.upstream_model);
        let connection = Connection::load(&state, connection_id).await.map_err(|_| {
            Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Cannot load the connection.",
            )
        })?;
        let estimate = crate::tokens::estimate(&body).await as i64;
        let reserved = reserve(&state, &key, estimate)?;
        let call = Call {
            key: key.clone(),
            route,
            connection_id,
            kind,
            route_name: "decisions",
            streamed: false,
            reserved,
            estimate: Tokens {
                input_text: estimate,
                ..Tokens::default()
            },
            media: Media::default(),
            inference_geo: Some(connection.decisions_geo()),
            started: Instant::now(),
            _permits: admission,
        };
        Ok::<_, Failure>((call, body, connection))
    }
    .await;
    let (call, body, connection) = match result {
        Ok(ok) => ok,
        Err(failure) => return error(failure),
    };
    let task_state = state.clone();
    detached(&state, call, async move {
        let request = match connection.request(&task_state, reqwest::Method::POST, "decisions", None) {
            Ok(request) => request,
            Err(err) => {
                return failed(Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream_blocked",
                    err.to_string(),
                ));
            }
        };
        let response = match send(request.json(&body), TOTAL_TIMEOUT).await {
            Ok(response) => response,
            Err(done) => return *done,
        };
        let answer = match read_json(response).await {
            Ok(answer) => answer,
            Err(failure) => return lost(failure),
        };
        let usage = &answer["usage"];
        let recorded = if usage["input_tokens"].as_i64().is_some_and(|n| n >= 0)
            && usage["output_tokens"].as_i64().is_some_and(|n| n >= 0)
        {
            Recorded::ok("reported", Tokens::from_openai(usage), Media::default())
        } else {
            Recorded::unknown()
        };
        (Json(answer).into_response(), Some(recorded))
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Embeddings

/// The input tokens of an embeddings request. An input can be token ids (one or more lists of
/// numbers); each number is one token.
async fn embedding_estimate(input: &Value) -> i64 {
    let items = input.as_array().map(Vec::as_slice).unwrap_or_default();
    let token_ids = |list: &[Value]| !list.is_empty() && list.iter().all(Value::is_number);
    if token_ids(items) {
        return items.len() as i64;
    }
    if !items.is_empty()
        && items
            .iter()
            .all(|item| item.as_array().is_some_and(|list| token_ids(list)))
    {
        return items
            .iter()
            .filter_map(Value::as_array)
            .map(|list| list.len() as i64)
            .sum();
    }
    crate::tokens::estimate(input).await as i64
}

pub async fn embeddings(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let result = async {
        let mut body = json_body(body)?;
        let (route, connection_id, kind) = target(
            &state,
            &key,
            &body["model"],
            &[Kind::OpenAi, Kind::OpenRouter],
            "Embeddings",
        )
        .await?;
        if kind == Kind::OpenRouter {
            provider::check_openrouter_routing(&body)?;
        }
        let estimate = embedding_estimate(&body["input"]).await;
        let reserved = reserve(&state, &key, estimate)?;
        body["model"] = json!(route.upstream_model);
        let call = Call {
            key: key.clone(),
            route,
            connection_id,
            kind,
            route_name: "embeddings",
            streamed: false,
            reserved,
            estimate: Tokens {
                input_text: estimate,
                ..Tokens::default()
            },
            media: Media::default(),
            inference_geo: None,
            started: Instant::now(),
            _permits: admission,
        };
        Ok::<_, Failure>((call, body))
    }
    .await;
    let (call, body) = match result {
        Ok(ok) => ok,
        Err(failure) => return error(failure),
    };
    let task_state = state.clone();
    let connection_id = call.connection_id;
    detached(&state, call, async move {
        let request = match upstream(&task_state, connection_id, "embeddings").await {
            Ok(request) => request,
            Err(failure) => return failed(failure),
        };
        let response = match send(request.json(&body), TOTAL_TIMEOUT).await {
            Ok(response) => response,
            Err(done) => return *done,
        };
        let answer = match read_json(response).await {
            Ok(answer) => answer,
            Err(failure) => return lost(failure),
        };
        let mut recorded = match answer["usage"]["prompt_tokens"].as_i64() {
            Some(input) => {
                let tokens = Tokens {
                    input_text: input,
                    ..Tokens::default()
                };
                Recorded::ok("reported", tokens, Media::default())
            }
            None => Recorded::unknown(),
        };
        // OpenRouter reports its cost in the usage.
        recorded.extras = Extras::from_usage(&answer["usage"]);
        (Json(answer).into_response(), Some(recorded))
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Speech

/// Audio output tokens per input character of the OpenAI speech models, for estimates.
const SPEECH_TOKENS_PER_CHARACTER: i64 = 4;

pub async fn speech(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let result = async {
        let mut body = json_body(body)?;
        let (route, connection_id, kind) =
            target(&state, &key, &body["model"], &[Kind::OpenAi], "Speech requests").await?;
        let input = body["input"]
            .as_str()
            .ok_or_else(|| bad("The field `input` is missing."))?
            .to_owned();
        // A custom voice is stored in the provider account, so other keys could use it.
        if body["voice"].is_object() {
            return Err(bad(
                "Custom voices (`voice.id`) are not supported. Use a built-in voice.",
            ));
        }
        let characters = input.chars().count() as i64;
        // The instructions are text input too; only the input is spoken.
        let estimate = crate::tokens::estimate(&json!([input, body["instructions"]])).await as i64;
        let audio = characters * SPEECH_TOKENS_PER_CHARACTER;
        let reserved = reserve(&state, &key, estimate + audio)?;
        body["model"] = json!(route.upstream_model);
        let streamed = body["stream_format"] == "sse";
        let call = Call {
            key: key.clone(),
            route,
            connection_id,
            kind,
            route_name: "audio_speech",
            streamed,
            reserved,
            estimate: Tokens {
                input_text: estimate,
                output_audio: audio,
                ..Tokens::default()
            },
            media: Media {
                characters,
                ..Media::default()
            },
            inference_geo: None,
            started: Instant::now(),
            _permits: admission,
        };
        Ok::<_, Failure>((call, body))
    }
    .await;
    let (call, body) = match result {
        Ok(ok) => ok,
        Err(failure) => return error(failure),
    };
    let task_state = state.clone();
    let stream_call = call.clone();
    detached(&state, call, async move {
        let request = match upstream(&task_state, stream_call.connection_id, "audio/speech").await {
            Ok(request) => request,
            Err(failure) => return failed(failure),
        };
        // Binary audio also comes in chunks, so the headers come at once.
        let response = match send(request.json(&body), provider::HEADERS_TIMEOUT).await {
            Ok(response) => response,
            Err(done) => return *done,
        };
        // Binary audio has no usage; SSE ends with an event that has it.
        let media = stream_call.media.clone();
        let finish = move |event: Option<Value>| match event.filter(|e| e["usage"].is_object()) {
            Some(event) => {
                let tokens = Tokens {
                    input_text: event["usage"]["input_tokens"].as_i64().unwrap_or(0),
                    output_audio: event["usage"]["output_tokens"].as_i64().unwrap_or(0),
                    ..Tokens::default()
                };
                Recorded::ok("reported", tokens, media)
            }
            None => Recorded::unknown(),
        };
        (forward(&task_state, stream_call, response, finish), None)
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Transcriptions and translations

/// One part of a multipart body.
struct Part {
    name: String,
    file_name: Option<String>,
    content_type: Option<String>,
    data: Bytes,
}

async fn read_parts(multipart: Result<Multipart, MultipartRejection>) -> Result<Vec<Part>, Failure> {
    let mut multipart = multipart.map_err(|rejection| bad(rejection.body_text()))?;
    let mut parts = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| bad(format!("The form is not valid: {err}")))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let data = field
            .bytes()
            .await
            .map_err(|err| bad(format!("The form is not valid: {err}")))?;
        parts.push(Part {
            name,
            file_name,
            content_type,
            data,
        });
    }
    Ok(parts)
}

fn text_part<'p>(parts: &'p [Part], name: &str) -> Option<&'p str> {
    parts
        .iter()
        .find(|p| p.name == name && p.file_name.is_none())
        .and_then(|p| std::str::from_utf8(&p.data).ok())
}

/// A new form with the upstream model name.
fn rebuild_form(parts: &[Part], model: &str) -> Result<reqwest::multipart::Form, Failure> {
    let mut form = reqwest::multipart::Form::new();
    for part in parts {
        if part.name == "model" {
            form = form.text("model", model.to_owned());
            continue;
        }
        match &part.file_name {
            Some(file_name) => {
                // The data is shared, not copied; the length keeps the part size known.
                let length = part.data.len() as u64;
                let mut file = reqwest::multipart::Part::stream_with_length(part.data.clone(), length)
                    .file_name(file_name.clone());
                if let Some(content_type) = &part.content_type {
                    file = file
                        .mime_str(content_type)
                        .map_err(|_| bad("A file has an invalid content type."))?;
                }
                form = form.part(part.name.clone(), file);
            }
            None => {
                let text = std::str::from_utf8(&part.data).map_err(|_| bad("A form field is not text."))?;
                form = form.text(part.name.clone(), text.to_owned());
            }
        }
    }
    Ok(form)
}

/// Audio files that are read at the same time. The reading runs on the blocking pool, where a
/// client that leaves cannot stop it. When all readers are busy, the length stays unknown.
static AUDIO_READERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// A file can have very many empty packets, so the reading has a time limit.
const MAX_AUDIO_READ: Duration = Duration::from_secs(5);

/// The file data for the audio reader. After the time limit every read fails, so the parser
/// also stops in work that reads.
struct TimedSource {
    data: std::io::Cursor<Bytes>,
    until: Instant,
}

impl std::io::Read for TimedSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() > self.until {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the audio read took too long",
            ));
        }
        self.data.read(buf)
    }
}

impl std::io::Seek for TimedSource {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.data.seek(position)
    }
}

impl symphonia::core::io::MediaSource for TimedSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.data.get_ref().len() as u64)
    }
}
/// Audio input tokens per second of the OpenAI audio models, for estimates.
const AUDIO_TOKENS_PER_SECOND: f64 = 10.0;

/// The length of an audio file in seconds. The packet lengths are added up (no decoding),
/// because a header can have no length (streamed WAV) or a wrong one.
fn audio_seconds(data: Bytes, file_name: Option<String>) -> Option<f64> {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let until = Instant::now() + MAX_AUDIO_READ;
    let source = TimedSource {
        data: std::io::Cursor::new(data),
        until,
    };
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = file_name
        .as_deref()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext)
    {
        hint.with_extension(extension);
    }
    let mut reader = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .ok()?;
    let track = reader.default_track(TrackType::Audio)?;
    let (id, time_base) = (track.id, track.time_base?);
    let (mut total, mut first, mut end) = (0_u64, i64::MAX, i64::MIN);
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            // A file without a length in its header (streamed WAV) ends like this.
            Err(symphonia::core::errors::Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            // A part of the length is worse than none: the file size then gives the estimate.
            Err(_) => return None,
        };
        if Instant::now() > until {
            return None;
        }
        if packet.track_id == id {
            let (pts, dur) = (packet.pts.get(), packet.dur.get());
            total = total.saturating_add(dur);
            first = first.min(pts);
            end = end.max(pts.saturating_add(i64::try_from(dur).unwrap_or(i64::MAX)));
        }
    }
    // WebM blocks can have no durations, but their timestamps move on.
    let span = u64::try_from(end.saturating_sub(first)).unwrap_or(0);
    let total = total.max(span);
    (total > 0)
        .then(|| time_base.calc_duration(symphonia::core::units::Duration::from(total)))
        .flatten()
        .map(|time| time.as_secs_f64())
}

/// Estimated audio tokens. Without a length, the file size at a low bit rate (16 kbit/s) gives
/// one, so a file that cannot be read still counts.
fn audio_estimate(seconds: Option<f64>, bytes: usize) -> i64 {
    let seconds = seconds.unwrap_or(bytes as f64 / 2_000.0);
    (seconds * AUDIO_TOKENS_PER_SECOND).ceil() as i64
}

/// The usage of a transcription when the answer has it: tokens or seconds.
fn transcription_usage(usage: Option<&Value>, seconds: Option<f64>) -> Recorded {
    let usage = usage.filter(|u| u.is_object());
    let (tokens, seconds, status) = match usage {
        Some(usage) if usage["type"] == "duration" => {
            (Tokens::default(), usage["seconds"].as_f64().or(seconds), "reported")
        }
        Some(usage) => {
            let details = &usage["input_token_details"];
            let input = usage["input_tokens"].as_i64().unwrap_or(0);
            let audio = details["audio_tokens"].as_i64().unwrap_or(input);
            let text = details["text_tokens"].as_i64().unwrap_or(input - audio);
            let tokens = Tokens {
                input_text: text.max(0),
                input_audio: audio.max(0),
                output_text: usage["output_tokens"].as_i64().unwrap_or(0),
                ..Tokens::default()
            };
            (tokens, seconds, "reported")
        }
        None => return Recorded::unknown(),
    };
    let media = Media {
        seconds,
        ..Media::default()
    };
    Recorded::ok(status, tokens, media)
}

async fn audio_text(
    state: AppState,
    key: ApiKey,
    admission: Admission,
    multipart: Result<Multipart, MultipartRejection>,
    path: &'static str,
    route_name: &'static str,
) -> Response {
    let result = async {
        let parts = read_parts(multipart).await?;
        let model = json!(text_part(&parts, "model"));
        let (route, connection_id, kind) = target(&state, &key, &model, &[Kind::OpenAi], "Audio requests").await?;
        let file = parts
            .iter()
            .find(|p| p.name == "file" && p.file_name.is_some())
            .ok_or_else(|| bad("The field `file` is missing."))?;
        let form = rebuild_form(&parts, &route.upstream_model)?;
        let (data, file_name, bytes) = (file.data.clone(), file.file_name.clone(), file.data.len());
        let seconds = match AUDIO_READERS.try_acquire() {
            Ok(permit) => tokio::task::spawn_blocking(move || {
                let _permit = permit;
                audio_seconds(data, file_name)
            })
            .await
            .ok()
            .flatten(),
            Err(_) => None,
        };
        // The answer has the real use; the audio tokens are estimated from the length.
        let estimate = audio_estimate(seconds, bytes);
        let reserved = reserve(&state, &key, estimate)?;
        let streamed = text_part(&parts, "stream") == Some("true");
        let call = Call {
            key: key.clone(),
            route,
            connection_id,
            kind,
            route_name,
            streamed,
            reserved,
            estimate: Tokens {
                input_audio: estimate,
                ..Tokens::default()
            },
            media: Media {
                seconds,
                ..Media::default()
            },
            inference_geo: None,
            started: Instant::now(),
            _permits: admission,
        };
        Ok::<_, Failure>((call, form, seconds))
    }
    .await;
    let (call, form, seconds) = match result {
        Ok(ok) => ok,
        Err(failure) => return error(failure),
    };
    let task_state = state.clone();
    let stream_call = call.clone();
    detached(&state, call, async move {
        let request = match upstream(&task_state, stream_call.connection_id, path).await {
            Ok(request) => request,
            Err(failure) => return failed(failure),
        };
        let response = match send(request.multipart(form), headers_limit(stream_call.streamed)).await {
            Ok(response) => response,
            Err(done) => return *done,
        };
        if stream_call.streamed {
            let finish = move |event: Option<Value>| transcription_usage(event.as_ref().map(|e| &e["usage"]), seconds);
            return (forward(&task_state, stream_call, response, finish), None);
        }
        // The answer is JSON or plain text (text, srt, vtt); it goes back unchanged.
        let content_type = content_type(&response);
        match read_body(response).await {
            Ok(body) => {
                let answer: Option<Value> = serde_json::from_slice(&body).ok();
                let recorded = transcription_usage(answer.as_ref().map(|a| &a["usage"]), seconds);
                (
                    with_content_type(Body::from(body).into_response(), &content_type),
                    Some(recorded),
                )
            }
            Err(failure) => lost(failure),
        }
    })
    .await
}

pub async fn transcriptions(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    audio_text(
        state,
        key,
        admission,
        multipart,
        "audio/transcriptions",
        "audio_transcription",
    )
    .await
}

pub async fn translations(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    audio_text(
        state,
        key,
        admission,
        multipart,
        "audio/translations",
        "audio_translation",
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Images

/// The usage of an Images API answer or of a completed stream event. Older models do not
/// return the size and quality, so the requested values are used then.
fn image_usage(answer: &Value, images: i64, requested: &Media) -> Recorded {
    let usage = &answer["usage"];
    let media = Media {
        images_generated: images,
        image_size: answer["size"]
            .as_str()
            .map(str::to_owned)
            .or(requested.image_size.clone()),
        image_quality: answer["quality"]
            .as_str()
            .map(str::to_owned)
            .or(requested.image_quality.clone()),
        input_images: requested.input_images,
        ..Media::default()
    };
    if usage.is_object() {
        Recorded::ok("reported", Tokens::from_image(usage, true), media)
    } else {
        Recorded::ok("estimated", Tokens::default(), media)
    }
}

/// Sends an Images API request to an OpenAI connection. Streams (`stream: true`) pass through.
async fn native_images(state: &AppState, call: Call, path: &'static str, body: ImageBody) -> Response {
    let task_state = state.clone();
    let stream_call = call.clone();
    detached(state, call, async move {
        let request = match upstream(&task_state, stream_call.connection_id, path).await {
            Ok(request) => request,
            Err(failure) => return failed(failure),
        };
        let request = match body {
            ImageBody::Json(body) => request.json(&body),
            ImageBody::Form(form) => request.multipart(form),
        };
        // An image stream sends its headers with the first image, which can take minutes.
        let response = match send(request, TOTAL_TIMEOUT).await {
            Ok(response) => response,
            Err(done) => return *done,
        };
        if stream_call.streamed {
            // A stream makes one image; its `completed` event has the usage.
            let requested = stream_call.media.clone();
            let finish = move |event: Option<Value>| {
                let completed = event.filter(|e| e["type"].as_str().is_some_and(|t| t.ends_with(".completed")));
                match completed {
                    Some(event) => image_usage(&event, 1, &requested),
                    None => Recorded::unknown(),
                }
            };
            return (forward(&task_state, stream_call, response, finish), None);
        }
        match read_json(response).await {
            Ok(answer) => {
                let images = answer["data"].as_array().map_or(0, |data| data.len() as i64);
                let recorded = image_usage(&answer, images, &stream_call.media);
                (Json(answer).into_response(), Some(recorded))
            }
            Err(failure) => lost(failure),
        }
    })
    .await
}

enum ImageBody {
    Json(Value),
    Form(reqwest::multipart::Form),
}

/// Image settings that the Responses image-generation tool takes from an Images API request.
const TOOL_FIELDS: [&str; 7] = [
    "size",
    "quality",
    "background",
    "output_format",
    "output_compression",
    "moderation",
    "input_fidelity",
];

/// Makes the image on ChatGPT: a Responses request with the image-generation tool, answered in
/// the Images API shape. The engine records the usage (the model and the image tool).
async fn chatgpt_images(
    state: &AppState,
    key: &ApiKey,
    admission: Admission,
    route_model: &str,
    request: &Value,
    images: Vec<String>,
) -> Response {
    if request["n"].as_i64().is_some_and(|n| n != 1) {
        return error(bad("ChatGPT models make one image per request (`n: 1`)."));
    }
    if request["response_format"].as_str().is_some_and(|f| f != "b64_json") {
        return error(bad("ChatGPT models return images only as `b64_json`."));
    }
    if request["stream"] == true {
        return error(bad("Streaming images is not supported for ChatGPT models."));
    }
    let Some(prompt) = request["prompt"].as_str() else {
        return error(bad("The field `prompt` is missing."));
    };
    let mut tool = json!({ "type": "image_generation" });
    for field in TOOL_FIELDS {
        if !request[field].is_null() {
            tool[field] = request[field].clone();
        }
    }
    let mut content = vec![json!({ "type": "input_text", "text": prompt })];
    if !images.is_empty() {
        tool["action"] = json!("edit");
        content.extend(
            images
                .into_iter()
                .map(|url| json!({ "type": "input_image", "image_url": url })),
        );
    }
    let body = json!({
        "model": route_model,
        "input": [{ "type": "message", "role": "user", "content": content }],
        "tools": [tool],
        "tool_choice": { "type": "image_generation" },
        "stream": false,
    });
    let incoming = request::Incoming {
        format: "responses",
        body: body.clone(),
        native: body,
        cache_hint: None,
        anthropic_beta: None,
        anthropic_version: None,
    };
    let mut job = match request::prepare(state, key, incoming, admission).await {
        Ok(prepared) => prepared.job,
        Err(failure) => return error(failure),
    };
    job.route_name = "images";
    job.client_format = "images";
    let mut rx = match engine::start(state, job).await {
        Ok(rx) => rx,
        Err(failure) => return error(failure),
    };
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Done(response) => {
                let answer = images_answer(&response);
                if answer["data"].as_array().is_none_or(Vec::is_empty) {
                    return error(no_image(&response));
                }
                return Json(answer).into_response();
            }
            Msg::Failed(failure) => return error(failure),
            Msg::Event(_) | Msg::Raw(_) | Msg::Native(_) => {}
        }
    }
    error(Failure::new(
        StatusCode::BAD_GATEWAY,
        "upstream_closed",
        "The request ended without an answer.",
    ))
}

/// The failure of a response without an image, with the text or refusal of the model.
fn no_image(response: &Value) -> Failure {
    let said: Vec<&str> = response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|part| part["text"].as_str().or(part["refusal"].as_str()))
        .collect();
    let reason = response["incomplete_details"]["reason"].as_str();
    let message = match (said.is_empty(), reason) {
        (false, _) => format!("The model made no image: {}", said.join(" ")),
        (true, Some(reason)) => format!("The model made no image ({reason})."),
        (true, None) => "The model made no image.".to_owned(),
    };
    Failure::new(StatusCode::BAD_GATEWAY, "no_image", message)
}

/// A Responses answer with image-generation calls in the Images API shape.
fn images_answer(response: &Value) -> Value {
    let calls: Vec<&Value> = response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "image_generation_call" && item["result"].is_string())
        .collect();
    let data: Vec<Value> = calls
        .iter()
        .map(|call| json!({ "b64_json": call["result"], "revised_prompt": call["revised_prompt"] }))
        .collect();
    let first = calls.first();
    let mut answer = json!({ "created": now(), "data": data });
    for field in ["size", "quality", "background", "output_format"] {
        if let Some(value) = first.map(|call| &call[field]).filter(|v| !v.is_null()) {
            answer[field] = value.clone();
        }
    }
    if response["tool_usage"]["image_gen"].is_object() {
        answer["usage"] = response["tool_usage"]["image_gen"].clone();
    }
    answer
}

pub async fn image_generations(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let mut body = match json_body(body) {
        Ok(body) => body,
        Err(failure) => return error(failure),
    };
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let route = match request::route(&state, &key, &model).await {
        Ok(route) => route,
        Err(failure) => return error(failure),
    };
    let (connection_id, kind) = match route.upstream {
        Upstream::ChatGpt => return chatgpt_images(&state, &key, admission, &model, &body, Vec::new()).await,
        Upstream::Connection { id, kind: Kind::OpenAi } => (id, Kind::OpenAi),
        Upstream::Connection { .. } => {
            return error(bad("Images work only with ChatGPT models and OpenAI connections."));
        }
    };
    let estimate = crate::tokens::estimate(&body["prompt"]).await as i64;
    let reserved = match reserve(&state, &key, estimate + request::DEFAULT_OUTPUT_RESERVE) {
        Ok(reserved) => reserved,
        Err(failure) => return error(failure),
    };
    body["model"] = json!(route.upstream_model);
    let call = Call {
        key,
        route,
        connection_id,
        kind,
        route_name: "images",
        streamed: body["stream"] == true,
        reserved,
        estimate: Tokens {
            input_text: estimate,
            ..Tokens::default()
        },
        media: Media {
            image_size: body["size"].as_str().map(str::to_owned),
            image_quality: body["quality"].as_str().map(str::to_owned),
            ..Media::default()
        },
        inference_geo: None,
        started: Instant::now(),
        _permits: admission,
    };
    native_images(&state, call, "images/generations", ImageBody::Json(body)).await
}

/// The Images API fields of a ChatGPT edit form, with the JSON types of the tool.
fn chatgpt_edit_request(parts: &[Part]) -> Result<Value, Failure> {
    let field = |name: &str| text_part(parts, name);
    let number = |name: &str| {
        field(name)
            .map(|value| {
                value
                    .parse::<i64>()
                    .map_err(|_| bad(format!("`{name}` must be a number.")))
            })
            .transpose()
    };
    let mut request = json!({
        "prompt": field("prompt"),
        "n": number("n")?,
        "response_format": field("response_format"),
        "stream": field("stream") == Some("true"),
    });
    for name in TOOL_FIELDS {
        request[name] = if name == "output_compression" {
            json!(number(name)?)
        } else {
            json!(field(name))
        };
    }
    Ok(request)
}

/// An uploaded input image of an edit request.
fn is_image_part(part: &Part) -> bool {
    part.file_name.is_some() && (part.name == "image" || part.name == "image[]")
}

pub async fn image_edits(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let parts = match read_parts(multipart).await {
        Ok(parts) => parts,
        Err(failure) => return error(failure),
    };
    let model = text_part(&parts, "model").unwrap_or_default().to_owned();
    let route = match request::route(&state, &key, &model).await {
        Ok(route) => route,
        Err(failure) => return error(failure),
    };
    match route.upstream {
        Upstream::ChatGpt => {
            if parts.iter().any(|p| p.name == "mask") {
                return error(bad("ChatGPT models do not take a `mask`."));
            }
            let request = match chatgpt_edit_request(&parts) {
                Ok(request) => request,
                Err(failure) => return error(failure),
            };
            let images: Vec<String> = parts
                .iter()
                .filter(|p| is_image_part(p))
                .map(|p| {
                    use base64::Engine;
                    let media_type = p.content_type.as_deref().unwrap_or("image/png");
                    format!(
                        "data:{media_type};base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&p.data)
                    )
                })
                .collect();
            if images.is_empty() {
                return error(bad("The field `image` is missing."));
            }
            chatgpt_images(&state, &key, admission, &model, &request, images).await
        }
        Upstream::Connection { id, kind: Kind::OpenAi } => {
            let form = match rebuild_form(&parts, &route.upstream_model) {
                Ok(form) => form,
                Err(failure) => return error(failure),
            };
            let field = |name: &str| text_part(&parts, name).map(str::to_owned);
            let prompt = field("prompt").unwrap_or_default();
            let estimate = crate::tokens::estimate(&json!(prompt)).await as i64;
            let reserved = match reserve(&state, &key, estimate + request::DEFAULT_OUTPUT_RESERVE) {
                Ok(reserved) => reserved,
                Err(failure) => return error(failure),
            };
            let call = Call {
                key,
                route,
                connection_id: id,
                kind: Kind::OpenAi,
                route_name: "images",
                streamed: field("stream").as_deref() == Some("true"),
                reserved,
                estimate: Tokens {
                    input_text: estimate,
                    ..Tokens::default()
                },
                media: Media {
                    image_size: field("size"),
                    image_quality: field("quality"),
                    input_images: parts.iter().filter(|p| is_image_part(p)).count() as i64,
                    ..Media::default()
                },
                inference_geo: None,
                started: Instant::now(),
                _permits: admission,
            };
            native_images(&state, call, "images/edits", ImageBody::Form(form)).await
        }
        Upstream::Connection { .. } => error(bad("Images work only with ChatGPT models and OpenAI connections.")),
    }
}
