//! Requests to keyed provider connections. When the upstream format is the client format, the
//! request and the answer pass through natively (the gateway reads the usage on the way).
//! Else the Responses request is translated, and a decoder turns the upstream stream into
//! Responses events for the client encoders.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::codex::error_text;
use super::engine::{
    Event, Failure, IDLE_TIMEOUT, Job, MAX_EVENT_BYTES, MAX_OUTPUT_BYTES, Msg, Outcome, limited_events, too_large,
};
use super::{sse, upstream_chat, upstream_messages};
use crate::connections::{Connection, Kind};
use crate::state::AppState;
use crate::usage::Tokens;

const HEADERS_TIMEOUT: Duration = Duration::from_secs(120);

/// The format of the upstream endpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Responses,
    Chat,
    Messages,
}

impl Wire {
    fn format(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
            Self::Messages => "messages",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat/completions",
            Self::Messages => "messages",
        }
    }
}

/// Picks the endpoint for the model. OpenAI models can have only one of the two endpoints, and
/// some reasoning models take function tools only on Responses. Unknown means both.
fn choose_wire(kind: Kind, client_format: &str, capabilities: &Value, body: &Value) -> Wire {
    match kind {
        Kind::Anthropic => Wire::Messages,
        Kind::OpenRouter => Wire::Chat,
        Kind::OpenAi => {
            let endpoints = capabilities["endpoints"].as_array();
            let has = |name: &str| endpoints.is_none_or(|list| list.iter().any(|e| e == name));
            let function_tools = body["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|t| t["type"] == "function"));
            let chat_fits = has("chat") && !(function_tools && capabilities["chat_tools"] == false);
            match client_format {
                "chat" if chat_fits => Wire::Chat,
                _ if has("responses") => Wire::Responses,
                _ => Wire::Chat,
            }
        }
    }
}

/// Turns upstream SSE events into Responses events.
pub trait Decoder: Send {
    /// One upstream event. A terminal Responses event ends the answer.
    fn push(&mut self, event: &str, data: &Value, out: &mut Vec<Event>) -> Result<(), Failure>;
    /// Usage in billing categories, once the answer ended.
    fn tokens(&self) -> Option<Tokens> {
        None
    }
    /// After a paused answer (Anthropic `pause_turn`): the assistant content to send back so
    /// that the upstream continues.
    fn continuation(&mut self) -> Option<Vec<Value>> {
        None
    }
    /// Ends a paused answer that does not continue.
    fn end(&mut self, _out: &mut Vec<Event>) {}
    fn output_tokens(&self) -> i64 {
        0
    }
    /// The complete upstream content of the answer (Anthropic blocks).
    fn assistant_content(&self) -> &[Value] {
        &[]
    }
}

/// Continuations after `pause_turn` for one answer.
const MAX_CONTINUATIONS: usize = 5;

/// Responses upstreams send Responses events already.
struct Identity;

impl Decoder for Identity {
    fn push(&mut self, event: &str, data: &Value, out: &mut Vec<Event>) -> Result<(), Failure> {
        let kind = data["type"].as_str().unwrap_or(event).to_owned();
        if kind == "response.failed" || kind == "error" {
            let error = if data["response"]["error"].is_object() {
                &data["response"]["error"]
            } else {
                &data["error"]
            };
            return Err(upstream_failure(
                error["message"].as_str().unwrap_or("The upstream failed."),
            ));
        }
        out.push(Event {
            kind,
            data: data.clone(),
        });
        Ok(())
    }
}

fn upstream_failure(message: &str) -> Failure {
    Failure::new(StatusCode::BAD_GATEWAY, "upstream_error", message)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn attempt(
    state: &AppState,
    job: &Job,
    connection_id: i64,
    kind: Kind,
    opened: &mut Option<oneshot::Sender<Result<(), Failure>>>,
    tx: &mpsc::Sender<Msg>,
    outcome: &mut Outcome,
    started: Instant,
) -> Result<(), Failure> {
    let connection = Connection::load(state, connection_id).await.map_err(|err| {
        tracing::error!(connection = connection_id, error = %err, "cannot load the connection");
        Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "Internal error.")
    })?;
    let wire = choose_wire(kind, job.client_format, &job.route.capabilities, &job.body);
    let native = wire.format() == job.client_format;
    // Translated requests always stream, so one decoder serves both client modes.
    let stream = !native || job.stream;
    let (mut body, betas) = if native {
        native_body(job, wire, kind)?
    } else {
        translated_body(state, job, wire, kind)?
    };
    let betas = merge_betas(betas, job.anthropic_beta.as_deref(), kind);

    let response = send(state, &connection, wire, &body, &betas, stream).await?;
    outcome.generation_started = true;
    if let Some(opened) = opened.take() {
        let _ = opened.send(Ok(()));
    }

    let ttl = cache_ttl(&body);
    if native && !stream {
        let answer = read_json(response, MAX_OUTPUT_BYTES).await?;
        record_native(wire, &answer, ttl.as_deref(), outcome);
        let _ = tx.send(Msg::Native(answer)).await;
        return Ok(());
    }
    if native {
        return match native_stream(response, wire, job, ttl.as_deref(), tx, outcome, started).await {
            // The client already has native frames, so the error goes out in the upstream format.
            Err(failure) if failure.code != super::engine::CLIENT_CLOSED => {
                outcome.status = failure.status.as_u16();
                outcome.error_kind = Some(failure.code.to_owned());
                for frame in native_error(wire, &failure) {
                    let _ = tx.send(Msg::Raw(frame)).await;
                }
                Ok(())
            }
            other => other,
        };
    }

    let mut decoder: Box<dyn Decoder> = match wire {
        Wire::Responses => Box::new(Identity),
        Wire::Chat => Box::new(upstream_chat::ChatDecoder::new(kind)),
        Wire::Messages => Box::new(upstream_messages::MessagesDecoder::new(ttl)),
    };
    let mut events = limited_events(response);
    let mut output_items: Vec<Value> = Vec::new();
    let mut output_bytes = 0usize;
    let mut continuations = 0;
    let client_max = body["max_tokens"].as_i64();
    // The request messages without the paused assistant turn; each continuation adds the
    // whole content so far once.
    let request_messages = job_messages(&body);
    loop {
        let mut produced = Vec::new();
        match tokio::time::timeout(IDLE_TIMEOUT, events.next()).await {
            Err(_) => {
                return Err(Failure::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "The upstream sent nothing for 5 minutes.",
                ));
            }
            Ok(None) => {
                // A paused Anthropic answer continues with its content as the assistant turn,
                // within the output limit that is left.
                let left = client_max.map(|max| max - decoder.output_tokens());
                let content = (continuations < MAX_CONTINUATIONS && left.is_none_or(|l| l > 0))
                    .then(|| decoder.continuation())
                    .flatten();
                match content {
                    Some(content) => {
                        continuations += 1;
                        let mut messages = request_messages.clone();
                        messages.push(json!({ "role": "assistant", "content": content }));
                        body["messages"] = Value::Array(messages);
                        if let Some(left) = left {
                            body["max_tokens"] = json!(left);
                            upstream_messages::fit_thinking_budget(&mut body);
                        }
                        let response = send(state, &connection, wire, &body, &betas, true).await?;
                        events = limited_events(response);
                        continue;
                    }
                    None => {
                        decoder.end(&mut produced);
                        if produced.is_empty() {
                            return Err(upstream_failure("The upstream closed the stream before the end."));
                        }
                    }
                }
            }
            Ok(Some(Err(err))) => return Err(upstream_failure(&format!("The upstream stream broke: {err}"))),
            Ok(Some(Ok(next))) => {
                if next.data.len() > MAX_EVENT_BYTES {
                    return Err(too_large("The upstream sent an event larger than 16 MB."));
                }
                // Chat streams end with the text `[DONE]`.
                let data = match serde_json::from_str::<Value>(&next.data) {
                    Ok(data) => data,
                    Err(_) if next.data.trim() == "[DONE]" => json!("[DONE]"),
                    Err(_) => continue,
                };
                decoder.push(&next.event, &data, &mut produced)?;
            }
        }
        for mut event in produced {
            if event.kind.ends_with(".delta") {
                outcome
                    .first_token_ms
                    .get_or_insert(started.elapsed().as_millis() as i64);
                outcome.streamed_chars += event.data["delta"].as_str().map_or(0, str::len);
            }
            if event.kind == "response.output_item.done" {
                output_bytes += event.data["item"].to_string().len();
                if output_bytes > MAX_OUTPUT_BYTES {
                    return Err(too_large("The answer is larger than 32 MB."));
                }
                if event.data["item"]["type"] == "web_search_call" && event.data["item"]["action"]["type"] == "search" {
                    outcome.web_search_calls += 1;
                }
                output_items.push(event.data["item"].clone());
            }
            let terminal = event.kind == "response.completed" || event.kind == "response.incomplete";
            if terminal {
                let empty = event.data["response"]["output"].as_array().is_none_or(Vec::is_empty);
                if empty && !output_items.is_empty() {
                    event.data["response"]["output"] = Value::Array(std::mem::take(&mut output_items));
                }
                outcome.final_response = Some(event.data["response"].clone());
                outcome.tokens = decoder.tokens();
                if job.client_format == "chat" {
                    state.thinking_cache.store(job.key.id, decoder.assistant_content());
                }
            }
            if tx.send(Msg::Event(event)).await.is_err() {
                return Err(super::engine::client_closed());
            }
            if terminal {
                let response = outcome.final_response.clone().unwrap_or_default();
                let _ = tx.send(Msg::Done(response)).await;
                return Ok(());
            }
        }
    }
}

/// Reads a JSON answer body with a size limit, within the headers timeout.
async fn read_json(response: reqwest::Response, limit: usize) -> Result<Value, Failure> {
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    let read = async {
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|err| upstream_failure(&format!("The upstream answer broke: {err}")))?;
            body.extend_from_slice(&chunk);
            if body.len() > limit {
                return Err(too_large("The answer is larger than the limit."));
            }
        }
        Ok(())
    };
    tokio::time::timeout(HEADERS_TIMEOUT, read).await.map_err(|_| {
        Failure::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "The upstream answer came too slowly.",
        )
    })??;
    serde_json::from_slice(&body).map_err(|_| upstream_failure("The upstream answer is not JSON."))
}

fn job_messages(body: &Value) -> Vec<Value> {
    body["messages"].as_array().cloned().unwrap_or_default()
}

/// The beta headers of the gateway plus the ones of an Anthropic client (for native requests).
fn merge_betas(mut betas: Vec<String>, client: Option<&str>, kind: Kind) -> Vec<String> {
    if kind == Kind::Anthropic {
        for beta in client
            .into_iter()
            .flat_map(|b| b.split(','))
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            if !betas.iter().any(|b| b == beta) {
                betas.push(beta.to_owned());
            }
        }
    }
    betas
}

async fn send(
    state: &AppState,
    connection: &Connection,
    wire: Wire,
    body: &Value,
    betas: &[String],
    stream: bool,
) -> Result<reqwest::Response, Failure> {
    let mut request = connection
        .request(state, reqwest::Method::POST, wire.path())
        .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))?
        .json(body);
    if stream {
        request = request.header("Accept", "text/event-stream");
    }
    if !betas.is_empty() {
        request = request.header("anthropic-beta", betas.join(","));
    }
    let response = tokio::time::timeout(HEADERS_TIMEOUT, request.send())
        .await
        .map_err(|_| {
            Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "The upstream did not answer in time.",
            )
        })?
        .map_err(|err| {
            Failure::new(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("Cannot reach the upstream: {err}"),
            )
        })?;
    if !response.status().is_success() {
        return Err(error_answer(response).await);
    }
    Ok(response)
}

/// A stream error event in the format of the upstream.
fn native_error(wire: Wire, failure: &Failure) -> Vec<bytes::Bytes> {
    match wire {
        Wire::Responses => {
            let data = json!({ "type": "error", "code": failure.code, "message": failure.message });
            vec![sse::frame(Some("error"), &data.to_string())]
        }
        Wire::Chat => {
            let data = json!({ "error": { "message": failure.message, "type": "server_error", "code": failure.code } });
            vec![sse::frame(None, &data.to_string()), sse::frame(None, "[DONE]")]
        }
        Wire::Messages => {
            let kind = match failure.status.as_u16() {
                429 => "rate_limit_error",
                503 | 529 => "overloaded_error",
                _ => "api_error",
            };
            let data = json!({ "type": "error", "error": { "type": kind, "message": failure.message } });
            vec![sse::frame(Some("error"), &data.to_string())]
        }
    }
}

/// Forwards a native stream and reads its usage.
async fn native_stream(
    response: reqwest::Response,
    wire: Wire,
    job: &Job,
    ttl: Option<&str>,
    tx: &mpsc::Sender<Msg>,
    outcome: &mut Outcome,
    started: Instant,
) -> Result<(), Failure> {
    let client_wants_usage = job.native["stream_options"]["include_usage"] == true;
    let mut events = limited_events(response);
    let mut tap = Tap::default();
    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, events.next()).await {
            Err(_) => {
                return Err(Failure::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "The upstream sent nothing for 5 minutes.",
                ));
            }
            Ok(None) if tap.ended => return Ok(()),
            Ok(None) => return Err(upstream_failure("The upstream closed the stream before the end.")),
            Ok(Some(Err(err))) => return Err(upstream_failure(&format!("The upstream stream broke: {err}"))),
            Ok(Some(Ok(event))) => event,
        };
        if next.data.len() > MAX_EVENT_BYTES {
            return Err(too_large("The upstream sent an event larger than 16 MB."));
        }
        let forward = tap.push(wire, &next.event, &next.data, client_wants_usage, ttl, outcome, started);
        if forward {
            let name = (next.event != "message").then_some(next.event.as_str());
            if tx.send(Msg::Raw(sse::frame(name, &next.data))).await.is_err() {
                return Err(super::engine::client_closed());
            }
        }
        if let Some(failure) = tap.failure.take() {
            // The client already has the error event; this only marks the usage row.
            outcome.status = failure.status.as_u16();
            outcome.error_kind = Some(failure.code.to_owned());
            return Ok(());
        }
        if tap.done {
            return Ok(());
        }
    }
}

/// Forwards `count_tokens` to an Anthropic connection.
pub(super) async fn count_tokens(
    state: &AppState,
    connection_id: i64,
    body: &Value,
    upstream_model: &str,
    beta: Option<&str>,
) -> Result<i64, Failure> {
    let connection = Connection::load(state, connection_id).await.map_err(|err| {
        tracing::error!(connection = connection_id, error = %err, "cannot load the connection");
        Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "Internal error.")
    })?;
    let mut body = body.clone();
    body["model"] = json!(upstream_model);
    let betas = merge_betas(Vec::new(), beta, Kind::Anthropic);
    let mut request = connection
        .request(state, reqwest::Method::POST, "messages/count_tokens")
        .map_err(|err| Failure::new(StatusCode::BAD_GATEWAY, "upstream_blocked", err.to_string()))?
        .json(&body);
    if !betas.is_empty() {
        request = request.header("anthropic-beta", betas.join(","));
    }
    let response = tokio::time::timeout(HEADERS_TIMEOUT, request.send())
        .await
        .map_err(|_| {
            Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "The upstream did not answer in time.",
            )
        })?
        .map_err(|err| {
            Failure::new(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("Cannot reach the upstream: {err}"),
            )
        })?;
    if !response.status().is_success() {
        return Err(error_answer(response).await);
    }
    let answer = read_json(response, 1024 * 1024).await?;
    answer["input_tokens"]
        .as_i64()
        .ok_or_else(|| upstream_failure("The upstream answer has no token count."))
}

/// Maps an upstream error answer. A refused provider key is the gateway's problem, not the
/// client's, so it becomes a 502.
async fn error_answer(response: reqwest::Response) -> Failure {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let text = error_text(response).await;
    let json: Value = serde_json::from_str(&text).unwrap_or_default();
    let message = json["error"]["message"]
        .as_str()
        .or_else(|| json["message"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| text.chars().take(300).collect());
    let mut failure = match status.as_u16() {
        401 | 403 => Failure::new(
            StatusCode::BAD_GATEWAY,
            "upstream_auth",
            format!("The provider refused the API key of the connection: {message}"),
        ),
        code => Failure::new(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
            "upstream_error",
            message,
        ),
    };
    failure.retry_after = retry_after;
    failure
}

/// The client body with the upstream model name, the alias defaults, and the fields the
/// gateway always sets.
fn native_body(job: &Job, wire: Wire, kind: Kind) -> Result<(Value, Vec<String>), Failure> {
    let mut body = job.native.clone();
    body["model"] = json!(job.route.upstream_model);
    // OpenRouter fallbacks and provider routing would pick models that the key may not use.
    for field in ["models", "provider"] {
        if kind == Kind::OpenRouter && !body[field].is_null() {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("`{field}` is not supported: it would skip the model list of the key."),
            ));
        }
    }
    // The prepared body has the alias defaults and the effort that the model supports.
    let effort = job.body["reasoning"]["effort"].as_str();
    let tier = job.body["service_tier"]
        .as_str()
        .map(|tier| if tier == "fast" { "priority" } else { tier });
    let mut betas = Vec::new();
    match wire {
        Wire::Responses => {
            if let Some(effort) = effort {
                body["reasoning"]["effort"] = json!(effort);
            }
            if let Some(summary) = job.body["reasoning"]["summary"].as_str() {
                body["reasoning"]["summary"] = json!(summary);
            }
            if let Some(tier) = tier {
                body["service_tier"] = json!(tier);
            }
            force_responses_fields(&mut body);
        }
        Wire::Chat => {
            if let Some(effort) = effort {
                match kind {
                    // An explicit token budget stays; else the effort field that the client used.
                    Kind::OpenRouter if body["reasoning"]["max_tokens"].is_null() => {
                        if body["reasoning_effort"].is_string() {
                            body["reasoning_effort"] = json!(effort);
                        } else {
                            body["reasoning"]["effort"] = json!(effort);
                        }
                    }
                    Kind::OpenRouter => {}
                    _ => body["reasoning_effort"] = json!(effort),
                }
            }
            if kind == Kind::OpenAi {
                if let Some(tier) = tier {
                    body["service_tier"] = json!(tier);
                }
                // No stored conversations under the shared provider account.
                body["store"] = json!(false);
            }
            if job.stream {
                body["stream_options"]["include_usage"] = json!(true);
            }
        }
        Wire::Messages => {
            betas = upstream_messages::prepare_native(&mut body, job.route.alias.as_ref(), &job.route.capabilities)?;
        }
    }
    Ok((body, betas))
}

fn translated_body(state: &AppState, job: &Job, wire: Wire, kind: Kind) -> Result<(Value, Vec<String>), Failure> {
    let bad = |message: String| Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message);
    match wire {
        Wire::Responses => {
            let mut body = job.body.clone();
            body["model"] = json!(job.route.upstream_model);
            body["stream"] = json!(true);
            if let Some(map) = body.as_object_mut() {
                map.remove("stop");
            }
            if body["service_tier"] == "fast" {
                body["service_tier"] = json!("priority");
            }
            force_responses_fields(&mut body);
            Ok((body, Vec::new()))
        }
        Wire::Chat => upstream_chat::encode(&job.body, &job.route, kind)
            .map(|body| (body, Vec::new()))
            .map_err(bad),
        Wire::Messages => {
            let restore = |id: &str| state.thinking_cache.get(job.key.id, id);
            upstream_messages::encode(&job.body, &job.route, &restore).map_err(bad)
        }
    }
}

/// No stored state at the provider, encrypted reasoning for the next turn, and no reasoning
/// that another provider made (OpenAI cannot read it).
fn force_responses_fields(body: &mut Value) {
    super::codex::drop_foreign_reasoning(body.get_mut("input"));
    body["store"] = json!(false);
    let mut include: Vec<Value> = body["include"].as_array().cloned().unwrap_or_default();
    if !include.iter().any(|v| v == "reasoning.encrypted_content") {
        include.push(json!("reasoning.encrypted_content"));
    }
    body["include"] = Value::Array(include);
}

/// The Anthropic cache time of a request: `Some` when all `cache_control` entries agree
/// (`5m` is the default), `None` when they are mixed.
fn cache_ttl(body: &Value) -> Option<String> {
    fn visit(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(control) = map.get("cache_control").filter(|c| c.is_object()) {
                    found.push(control["ttl"].as_str().unwrap_or("5m").to_owned());
                }
                map.values().for_each(|v| visit(v, found));
            }
            Value::Array(items) => items.iter().for_each(|v| visit(v, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    visit(body, &mut found);
    found.dedup();
    match found.as_slice() {
        [] => Some("5m".into()),
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Usage of a native answer without streaming.
fn record_native(wire: Wire, answer: &Value, ttl: Option<&str>, outcome: &mut Outcome) {
    match wire {
        Wire::Responses => {
            outcome.web_search_calls = answer["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "web_search_call" && item["action"]["type"] == "search")
                .count() as i64;
            outcome.final_response = Some(answer.clone());
        }
        Wire::Chat => {
            if answer["usage"].is_object() {
                outcome.tokens = Some(upstream_chat::tokens(&answer["usage"]));
            }
            outcome.service_tier = answer["service_tier"].as_str().map(str::to_owned);
        }
        Wire::Messages => {
            outcome.service_tier = answer["usage"]["service_tier"].as_str().map(str::to_owned);
            if answer["usage"].is_object() {
                outcome.tokens = Some(Tokens::from_anthropic(&answer["usage"], ttl));
                outcome.web_search_calls = answer["usage"]["server_tool_use"]["web_search_requests"]
                    .as_i64()
                    .unwrap_or(0);
            }
        }
    }
}

/// Reads usage from a native stream on the way to the client.
#[derive(Default)]
struct Tap {
    /// The answer is complete; the upstream may still close the stream.
    ended: bool,
    /// The stream is complete.
    done: bool,
    failure: Option<Failure>,
    /// Anthropic usage: `message_start` has the input, `message_delta` the final output.
    anthropic_usage: Value,
}

impl Tap {
    /// Returns false for an event that the client must not get.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        wire: Wire,
        event: &str,
        data: &str,
        client_wants_usage: bool,
        ttl: Option<&str>,
        outcome: &mut Outcome,
        started: Instant,
    ) -> bool {
        if wire == Wire::Chat && data.trim() == "[DONE]" {
            self.ended = true;
            self.done = true;
            return true;
        }
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            return true;
        };
        let mut text = |delta: &Value| {
            if let Some(delta) = delta.as_str().filter(|d| !d.is_empty()) {
                outcome
                    .first_token_ms
                    .get_or_insert(started.elapsed().as_millis() as i64);
                outcome.streamed_chars += delta.len();
            }
        };
        match wire {
            Wire::Responses => {
                let kind = json["type"].as_str().unwrap_or(event);
                if kind.ends_with(".delta") {
                    text(&json["delta"]);
                }
                if kind == "response.output_item.done"
                    && json["item"]["type"] == "web_search_call"
                    && json["item"]["action"]["type"] == "search"
                {
                    outcome.web_search_calls += 1;
                }
                match kind {
                    "response.completed" | "response.incomplete" => {
                        outcome.final_response = Some(json["response"].clone());
                        self.ended = true;
                        self.done = true;
                    }
                    "response.failed" | "error" => {
                        self.failure = Some(upstream_failure("The upstream failed."));
                    }
                    _ => {}
                }
                true
            }
            Wire::Chat => {
                if json["error"].is_object() {
                    self.failure = Some(upstream_failure("The upstream failed."));
                    return true;
                }
                for choice in json["choices"].as_array().into_iter().flatten() {
                    if !choice["finish_reason"].is_null() {
                        self.ended = true;
                    }
                    text(&choice["delta"]["content"]);
                    text(&choice["delta"]["reasoning_content"]);
                    text(&choice["delta"]["reasoning"]);
                    for call in choice["delta"]["tool_calls"].as_array().into_iter().flatten() {
                        text(&call["function"]["arguments"]);
                    }
                }
                if let Some(tier) = json["service_tier"].as_str() {
                    outcome.service_tier = Some(tier.to_owned());
                }
                if json["usage"].is_object() {
                    outcome.tokens = Some(upstream_chat::tokens(&json["usage"]));
                    // The gateway asked for the usage chunk; a client that did not gets no
                    // chunk without choices.
                    let only_usage = json["choices"].as_array().is_none_or(Vec::is_empty);
                    return client_wants_usage || !only_usage;
                }
                true
            }
            Wire::Messages => {
                match json["type"].as_str().unwrap_or(event) {
                    "message_start" => {
                        self.anthropic_usage = json["message"]["usage"].clone();
                        outcome.service_tier = self.anthropic_usage["service_tier"].as_str().map(str::to_owned);
                    }
                    "content_block_delta" => {
                        text(&json["delta"]["text"]);
                        text(&json["delta"]["thinking"]);
                        text(&json["delta"]["partial_json"]);
                    }
                    "message_delta" => {
                        // The delta has the final output count and can update the input counts.
                        for (key, value) in json["usage"].as_object().into_iter().flatten() {
                            if !value.is_null() {
                                self.anthropic_usage[key] = value.clone();
                            }
                        }
                        outcome.tokens = Some(Tokens::from_anthropic(&self.anthropic_usage, ttl));
                        outcome.web_search_calls = self.anthropic_usage["server_tool_use"]["web_search_requests"]
                            .as_i64()
                            .unwrap_or(0);
                    }
                    "message_stop" => {
                        self.ended = true;
                        self.done = true;
                    }
                    "error" => self.failure = Some(upstream_failure("The upstream failed.")),
                    _ => {}
                }
                true
            }
        }
    }
}
