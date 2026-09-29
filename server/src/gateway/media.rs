//! Embeddings, audio and images. These go natively to OpenAI or OpenRouter connections; images
//! for ChatGPT models run through the Responses image-generation tool. Each call runs in its own
//! task, so a client that leaves does not lose the usage row.

use std::time::Instant;

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
use super::{provider, request};
use crate::connections::{Connection, Kind};
use crate::crypto::random_token;
use crate::db::now;
use crate::state::AppState;
use crate::usage::{Media, Row, Tokens};

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
    started: Instant,
    _permits: Admission,
}

/// The usage of a finished call.
struct Recorded {
    status: u16,
    error_kind: Option<String>,
    usage_status: &'static str,
    tokens: Tokens,
    media: Media,
}

impl Recorded {
    fn failed(failure: &Failure) -> Self {
        Self {
            status: failure.status.as_u16(),
            error_kind: Some(failure.code.to_owned()),
            usage_status: "none",
            tokens: Tokens::default(),
            media: Media::default(),
        }
    }
}

fn bad(message: impl Into<String>) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message)
}

async fn record(state: &AppState, call: &Call, recorded: Recorded) {
    // Without reported usage the real use is unknown, so keep at least the reservation.
    let used = recorded.tokens.input() + recorded.tokens.output();
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
        failover_attempts: 0,
        media: recorded.media,
    };
    state.usage.record(row).await;
}

/// Runs the work in its own task. The work returns the response and its usage, or no usage
/// when a stream records it later. A timeout or shutdown records a failed row.
async fn detached<F>(state: &AppState, call: Call, work: F) -> Response
where
    F: Future<Output = (Response, Option<Recorded>)> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let task_state = state.clone();
    state.gateway_tasks.clone().spawn(async move {
        let result = tokio::select! {
            result = tokio::time::timeout(TOTAL_TIMEOUT, work) => result.map_err(|_| {
                Failure::new(StatusCode::GATEWAY_TIMEOUT, "timeout", "The request took longer than one hour.")
            }),
            () = task_state.stopping.cancelled() => Err(Failure::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "The server is stopping.",
            )),
        };
        let response = match result {
            Ok((response, recorded)) => {
                if let Some(recorded) = recorded {
                    record(&task_state, &call, recorded).await;
                }
                response
            }
            Err(failure) => {
                record(&task_state, &call, Recorded::failed(&failure)).await;
                error(failure)
            }
        };
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

async fn load(state: &AppState, connection_id: i64) -> Result<Connection, Failure> {
    Connection::load(state, connection_id).await.map_err(|err| {
        tracing::error!(connection = connection_id, error = %err, "cannot load the connection");
        Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "Internal error.")
    })
}

/// Sends the request and maps an error answer.
async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, Failure> {
    let response = request.send().await.map_err(|err| {
        Failure::new(
            StatusCode::BAD_GATEWAY,
            "upstream_unreachable",
            format!("Cannot reach the upstream: {err}"),
        )
    })?;
    if !response.status().is_success() {
        return Err(provider::error_answer(response).await);
    }
    Ok(response)
}

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned()
}

/// Reads a whole answer body with the size limit and the idle timeout.
async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout(IDLE_TIMEOUT, chunks.next()).await.map_err(|_| {
            Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "The upstream sent nothing for 5 minutes.",
            )
        })?;
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|err| {
            Failure::new(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                format!("The upstream answer broke: {err}"),
            )
        })?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_OUTPUT_BYTES {
            return Err(engine::too_large("The answer is larger than 32 MB."));
        }
    }
    Ok(body)
}

fn bytes_response(content_type: &str, body: Vec<u8>) -> Response {
    let mut response = Body::from(body).into_response();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// Forwards a streamed answer (SSE or binary audio) as it is. `data:` lines with a `usage`
/// object are read on the way; the usage row is written when the stream ends.
fn forward(
    state: &AppState,
    call: Call,
    response: reqwest::Response,
    finish: impl FnOnce(Option<Value>) -> Recorded + Send + 'static,
) -> Response {
    let content_type = content_type(&response);
    let task_type = content_type.clone();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let task_state = state.clone();
    state.gateway_tasks.clone().spawn(async move {
        let mut chunks = response.bytes_stream();
        let sse = task_type.starts_with("text/event-stream");
        let mut line = Vec::new();
        let mut too_long = false;
        let mut usage = None;
        let mut failure = None;
        loop {
            let next = tokio::select! {
                next = tokio::time::timeout(IDLE_TIMEOUT, chunks.next()) => next,
                () = tx.closed() => {
                    failure = Some(engine::client_closed());
                    break;
                }
                () = task_state.stopping.cancelled() => {
                    failure = Some(Failure::new(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", "The server is stopping."));
                    break;
                }
            };
            let chunk = match next {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(None) => break,
                Ok(Some(Err(err))) => {
                    failure = Some(Failure::new(
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        format!("The upstream answer broke: {err}"),
                    ));
                    break;
                }
                Err(_) => {
                    failure = Some(Failure::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream_timeout",
                        "The upstream sent nothing for 5 minutes.",
                    ));
                    break;
                }
            };
            if sse {
                let mut pieces = chunk.split(|byte| *byte == b'\n').peekable();
                while let Some(piece) = pieces.next() {
                    if line.len() + piece.len() <= MAX_EVENT_BYTES {
                        line.extend_from_slice(piece);
                    } else {
                        line.clear();
                        too_long = true;
                    }
                    // The last piece has no line end yet.
                    if pieces.peek().is_none() {
                        break;
                    }
                    if !too_long
                        && let Some(data) = line.strip_prefix(b"data: ")
                        && let Ok(event) = serde_json::from_slice::<Value>(data)
                        && event["usage"].is_object()
                    {
                        usage = Some(event["usage"].clone());
                    }
                    line.clear();
                    too_long = false;
                }
            }
            if tx.send(Ok(chunk)).await.is_err() {
                failure = Some(engine::client_closed());
                break;
            }
        }
        let mut recorded = finish(usage);
        if let Some(failure) = failure {
            recorded.status = failure.status.as_u16();
            recorded.error_kind = Some(failure.code.to_owned());
        }
        record(&task_state, &call, recorded).await;
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut response = Body::from_stream(stream).into_response();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
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
// Embeddings

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
        let estimate = crate::tokens::estimate(&body["input"]).await as i64;
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
            started: Instant::now(),
            _permits: admission,
        };
        Ok::<_, Failure>((call, body, estimate))
    }
    .await;
    let (call, body, estimate) = match result {
        Ok(ok) => ok,
        Err(failure) => return error(failure),
    };
    let task_state = state.clone();
    let connection_id = call.connection_id;
    detached(&state, call, async move {
        let answer = async {
            let connection = load(&task_state, connection_id).await?;
            let request = connection
                .request(&task_state, reqwest::Method::POST, "embeddings", None)
                .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))?;
            let response = send(request.json(&body)).await?;
            let answer: Value = serde_json::from_slice(&read_body(response).await?).map_err(|_| {
                Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "The upstream answer is not JSON.",
                )
            })?;
            Ok::<_, Failure>(answer)
        }
        .await;
        match answer {
            Ok(answer) => {
                let reported = answer["usage"]["prompt_tokens"].as_i64();
                let recorded = Recorded {
                    status: 200,
                    error_kind: None,
                    usage_status: if reported.is_some() { "reported" } else { "estimated" },
                    tokens: Tokens {
                        input_text: reported.unwrap_or(estimate),
                        ..Tokens::default()
                    },
                    media: Media::default(),
                };
                (Json(answer).into_response(), Some(recorded))
            }
            Err(failure) => {
                let recorded = Recorded::failed(&failure);
                (error(failure), Some(recorded))
            }
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Speech

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
        let estimate = crate::tokens::estimate(&json!(input)).await as i64;
        let reserved = reserve(&state, &key, estimate)?;
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
            started: Instant::now(),
            _permits: admission,
        };
        Ok::<_, Failure>((call, body, input.chars().count() as i64, estimate))
    }
    .await;
    let (call, body, characters, estimate) = match result {
        Ok(ok) => ok,
        Err(failure) => return error(failure),
    };
    let task_state = state.clone();
    let stream_call = call.clone();
    detached(&state, call, async move {
        let response = async {
            let connection = load(&task_state, stream_call.connection_id).await?;
            let request = connection
                .request(&task_state, reqwest::Method::POST, "audio/speech", None)
                .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))?;
            send(request.json(&body)).await
        }
        .await;
        match response {
            // Binary audio has no usage; SSE ends with an event that has it.
            Ok(response) => {
                let finish = move |usage: Option<Value>| {
                    let media = Media {
                        characters,
                        ..Media::default()
                    };
                    match usage {
                        Some(usage) => Recorded {
                            status: 200,
                            error_kind: None,
                            usage_status: "reported",
                            tokens: Tokens {
                                input_text: usage["input_tokens"].as_i64().unwrap_or(0),
                                output_audio: usage["output_tokens"].as_i64().unwrap_or(0),
                                ..Tokens::default()
                            },
                            media,
                        },
                        None => Recorded {
                            status: 200,
                            error_kind: None,
                            usage_status: "estimated",
                            tokens: Tokens {
                                input_text: estimate,
                                ..Tokens::default()
                            },
                            media,
                        },
                    }
                };
                (forward(&task_state, stream_call, response, finish), None)
            }
            Err(failure) => {
                let recorded = Recorded::failed(&failure);
                (error(failure), Some(recorded))
            }
        }
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

/// The length of an audio file in seconds. The packet lengths are added up (no decoding),
/// because a header can have no length (streamed WAV) or a wrong one.
fn audio_seconds(data: Bytes, file_name: Option<String>) -> Option<f64> {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::units::Duration;

    let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
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
    let mut total: u64 = 0;
    while let Ok(Some(packet)) = reader.next_packet() {
        if packet.track_id == id {
            total = total.saturating_add(packet.dur.get());
        }
    }
    (total > 0)
        .then(|| time_base.calc_duration(Duration::from(total)))
        .flatten()
        .map(|time| time.as_secs_f64())
}

/// The usage of a transcription: tokens or seconds when the answer has them, else the file
/// length.
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
        None => (
            Tokens::default(),
            seconds,
            if seconds.is_some() { "reported" } else { "estimated" },
        ),
    };
    Recorded {
        status: 200,
        error_kind: None,
        usage_status: status,
        tokens,
        media: Media {
            seconds,
            ..Media::default()
        },
    }
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
        let (data, file_name) = (file.data.clone(), file.file_name.clone());
        let seconds = tokio::task::spawn_blocking(move || audio_seconds(data, file_name))
            .await
            .ok()
            .flatten();
        let form = rebuild_form(&parts, &route.upstream_model)?;
        let streamed = text_part(&parts, "stream") == Some("true");
        // The cost is known only after the answer, so nothing is reserved.
        let call = Call {
            key: key.clone(),
            route,
            connection_id,
            kind,
            route_name,
            streamed,
            reserved: 0,
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
        let response = async {
            let connection = load(&task_state, stream_call.connection_id).await?;
            let request = connection
                .request(&task_state, reqwest::Method::POST, path, None)
                .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))?;
            send(request.multipart(form)).await
        }
        .await;
        let response = match response {
            Ok(response) => response,
            Err(failure) => {
                let recorded = Recorded::failed(&failure);
                return (error(failure), Some(recorded));
            }
        };
        if stream_call.streamed {
            let finish = move |usage: Option<Value>| transcription_usage(usage.as_ref(), seconds);
            return (forward(&task_state, stream_call, response, finish), None);
        }
        // The answer is JSON or plain text (text, srt, vtt); it goes back unchanged.
        let content_type = content_type(&response);
        match read_body(response).await {
            Ok(body) => {
                let answer: Option<Value> = serde_json::from_slice(&body).ok();
                let recorded = transcription_usage(answer.as_ref().map(|a| &a["usage"]), seconds);
                (bytes_response(&content_type, body), Some(recorded))
            }
            Err(failure) => {
                let recorded = Recorded::failed(&failure);
                (error(failure), Some(recorded))
            }
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

/// The usage of an Images API answer.
fn image_usage(answer: &Value, size: Option<&str>, quality: Option<&str>) -> Recorded {
    let usage = &answer["usage"];
    let media = Media {
        images_generated: answer["data"].as_array().map_or(0, |data| data.len() as i64),
        image_size: answer["size"].as_str().or(size).map(str::to_owned),
        image_quality: answer["quality"].as_str().or(quality).map(str::to_owned),
        ..Media::default()
    };
    Recorded {
        status: 200,
        error_kind: None,
        usage_status: if usage.is_object() { "reported" } else { "estimated" },
        tokens: if usage.is_object() {
            Tokens::from_image(usage, true)
        } else {
            Tokens::default()
        },
        media,
    }
}

/// Sends an Images API request to an OpenAI connection. Streams (`stream: true`) pass through.
async fn native_images(
    state: &AppState,
    call: Call,
    path: &'static str,
    body: ImageBody,
    size: Option<String>,
    quality: Option<String>,
) -> Response {
    let task_state = state.clone();
    let stream_call = call.clone();
    detached(state, call, async move {
        let response = async {
            let connection = load(&task_state, stream_call.connection_id).await?;
            let request = connection
                .request(&task_state, reqwest::Method::POST, path, None)
                .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))?;
            let request = match body {
                ImageBody::Json(body) => request.json(&body),
                ImageBody::Form(form) => request.multipart(form),
            };
            send(request).await
        }
        .await;
        let response = match response {
            Ok(response) => response,
            Err(failure) => {
                let recorded = Recorded::failed(&failure);
                return (error(failure), Some(recorded));
            }
        };
        if stream_call.streamed {
            let finish = move |usage: Option<Value>| {
                let answer = json!({ "usage": usage, "data": [{}] });
                image_usage(&answer, size.as_deref(), quality.as_deref())
            };
            return (forward(&task_state, stream_call, response, finish), None);
        }
        match read_body(response).await {
            Ok(body) => match serde_json::from_slice::<Value>(&body) {
                Ok(answer) => {
                    let recorded = image_usage(&answer, size.as_deref(), quality.as_deref());
                    (Json(answer).into_response(), Some(recorded))
                }
                Err(_) => {
                    let failure = Failure::new(
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        "The upstream answer is not JSON.",
                    );
                    let recorded = Recorded::failed(&failure);
                    (error(failure), Some(recorded))
                }
            },
            Err(failure) => {
                let recorded = Recorded::failed(&failure);
                (error(failure), Some(recorded))
            }
        }
    })
    .await
}

enum ImageBody {
    Json(Value),
    Form(reqwest::multipart::Form),
}

/// Image settings that the Responses image-generation tool takes from an Images API request.
const TOOL_FIELDS: [&str; 6] = [
    "size",
    "quality",
    "background",
    "output_format",
    "output_compression",
    "moderation",
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
            Msg::Done(response) => return Json(images_answer(&response)).into_response(),
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
    let estimate = crate::tokens::estimate(&body["prompt"]).await as i64 + request::DEFAULT_OUTPUT_RESERVE;
    let reserved = match reserve(&state, &key, estimate) {
        Ok(reserved) => reserved,
        Err(failure) => return error(failure),
    };
    body["model"] = json!(route.upstream_model);
    let size = body["size"].as_str().map(str::to_owned);
    let quality = body["quality"].as_str().map(str::to_owned);
    let streamed = body["stream"] == true;
    let call = Call {
        key,
        route,
        connection_id,
        kind,
        route_name: "images",
        streamed,
        reserved,
        started: Instant::now(),
        _permits: admission,
    };
    native_images(&state, call, "images/generations", ImageBody::Json(body), size, quality).await
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
    let field = |name: &str| text_part(&parts, name).map(str::to_owned);
    match route.upstream {
        Upstream::ChatGpt => {
            if parts.iter().any(|p| p.name == "mask") {
                return error(bad("ChatGPT models do not take a `mask`."));
            }
            let images: Vec<String> = parts
                .iter()
                .filter(|p| p.file_name.is_some() && (p.name == "image" || p.name == "image[]"))
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
            let mut request = json!({ "prompt": field("prompt"), "n": field("n").and_then(|n| n.parse::<i64>().ok()) });
            for name in TOOL_FIELDS {
                if let Some(value) = field(name) {
                    request[name] = json!(value);
                }
            }
            if let Some(format) = field("response_format") {
                request["response_format"] = json!(format);
            }
            chatgpt_images(&state, &key, admission, &model, &request, images).await
        }
        Upstream::Connection { id, kind: Kind::OpenAi } => {
            let prompt = field("prompt").unwrap_or_default();
            let estimate = crate::tokens::estimate(&json!(prompt)).await as i64 + request::DEFAULT_OUTPUT_RESERVE;
            let reserved = match reserve(&state, &key, estimate) {
                Ok(reserved) => reserved,
                Err(failure) => return error(failure),
            };
            let form = match rebuild_form(&parts, &route.upstream_model) {
                Ok(form) => form,
                Err(failure) => return error(failure),
            };
            let streamed = field("stream").as_deref() == Some("true");
            let (size, quality) = (field("size"), field("quality"));
            let call = Call {
                key,
                route,
                connection_id: id,
                kind: Kind::OpenAi,
                route_name: "images",
                streamed,
                reserved,
                started: Instant::now(),
                _permits: admission,
            };
            native_images(&state, call, "images/edits", ImageBody::Form(form), size, quality).await
        }
        Upstream::Connection { .. } => error(bad("Images work only with ChatGPT models and OpenAI connections.")),
    }
}
