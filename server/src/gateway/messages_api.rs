//! `POST /v1/messages` and `/v1/messages/count_tokens`: Anthropic Messages clients such as
//! Claude Code, translated to and from Responses.

use std::collections::HashMap;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::auth::{Admission, ApiKey};
use super::engine::{self, Failure, Msg};
use super::error::{ErrorFormat, GatewayError};
use super::{request, sse};
use crate::db::now;
use crate::state::AppState;
use crate::usage::{Row, Tokens};

/// OpenAI reasoning travels in the `signature` of an Anthropic thinking block with this prefix.
/// Signatures without it come from Anthropic and are dropped.
const SIGNATURE_PREFIX: &str = "aip1:";

pub async fn create(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return error(request::bad_json(&rejection)),
    };
    let converted = match to_responses(&body) {
        Ok(converted) => converted,
        Err(message) => return error(Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message)),
    };
    let show_thinking = body["thinking"]["display"].as_str() != Some("omitted");
    let hint = headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok());
    let prepared =
        match request::prepare(&state, &key, converted, "messages", "messages", hint, admission.clone()).await {
            Ok(prepared) => prepared,
            Err(failure) => return error(failure),
        };
    let stream = prepared.job.stream;
    let requested = prepared.job.route.requested.clone();
    let rx = match engine::start(&state, prepared.job).await {
        Ok(rx) => rx,
        Err(failure) => return error(failure),
    };
    if stream {
        let mut encoder = EventEncoder::new(requested, show_thinking);
        return sse::response(
            rx,
            move |msg| encoder.encode(msg),
            "event: ping\ndata: {\"type\": \"ping\"}\n\n",
            admission,
        );
    }
    collect(rx, &requested, show_thinking).await
}

/// Local estimate: the Anthropic count needs an Anthropic model. It uses no generation
/// allowance, but it has a usage row with zero tokens.
pub async fn count_tokens(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return error(request::bad_json(&rejection)),
    };
    let started = std::time::Instant::now();
    let converted = match to_responses(&body) {
        Ok(converted) => converted,
        Err(message) => return error(Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message)),
    };
    let requested = body["model"].as_str().unwrap_or_default();
    let route = match request::route(&state, &key, requested).await {
        Ok(route) => route,
        Err(failure) => return error(failure),
    };
    let input_tokens = crate::tokens::estimate(&converted).await;
    state
        .usage
        .record(Row {
            request_id: crate::crypto::random_token(12),
            time: now(),
            api_key_id: key.id,
            route: "count_tokens",
            client_format: "messages",
            upstream: "local",
            chatgpt_account_id: None,
            requested_model: route.requested,
            resolved_model: Some(route.qualified),
            effort: None,
            service_tier_requested: None,
            service_tier_reported: None,
            streamed: false,
            status_code: 200,
            error_kind: None,
            latency_ms: started.elapsed().as_millis() as i64,
            first_token_ms: None,
            usage_status: "none",
            tokens: Tokens::default(),
            web_search_calls: 0,
            failover_attempts: 0,
        })
        .await;
    Json(json!({ "input_tokens": input_tokens })).into_response()
}

pub fn error(failure: Failure) -> Response {
    let mut error = GatewayError::new(ErrorFormat::Anthropic, failure.status, failure.code, failure.message);
    if let Some(seconds) = failure.retry_after {
        error = error.with_retry_after(seconds);
    }
    error.into_response()
}

// ---------------------------------------------------------------------------------------------
// Request

pub fn to_responses(body: &Value) -> Result<Value, String> {
    let messages = body["messages"].as_array().ok_or("The field `messages` is missing.")?;
    let mut input: Vec<Value> = Vec::new();
    // Claude Code can also put system text into `messages`.
    let mut extra_system: Vec<String> = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        let blocks = match &message["content"] {
            Value::String(text) => vec![json!({ "type": "text", "text": text })],
            Value::Array(blocks) => blocks.clone(),
            _ => Vec::new(),
        };
        match role {
            "user" => user_message(&blocks, &mut input)?,
            "assistant" => assistant_message(&blocks, &mut input),
            "system" => extra_system.extend(blocks.iter().filter_map(|b| b["text"].as_str()).map(str::to_owned)),
            other => return Err(format!("Unknown message role `{other}`.")),
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), body["model"].clone());
    out.insert("input".into(), Value::Array(input));
    let mut system: Vec<String> = match &body["system"] {
        Value::String(text) => vec![text.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    system.extend(extra_system);
    let system = system.join("\n\n");
    if !system.is_empty() {
        out.insert("instructions".into(), json!(system));
    }
    out.insert("stream".into(), json!(body["stream"].as_bool().unwrap_or(false)));
    if let Some(tools) = body["tools"].as_array() {
        out.insert(
            "tools".into(),
            Value::Array(tools.iter().map(tool).collect::<Result<_, _>>()?),
        );
    }
    let tool_choice = &body["tool_choice"];
    match tool_choice["type"].as_str() {
        Some("auto") => {
            out.insert("tool_choice".into(), json!("auto"));
        }
        Some("any") => {
            out.insert("tool_choice".into(), json!("required"));
        }
        Some("none") => {
            out.insert("tool_choice".into(), json!("none"));
        }
        Some("tool") => {
            // A named choice of the web search tool selects the hosted search.
            let searches = body["tools"].as_array().into_iter().flatten().any(|t| {
                t["name"] == tool_choice["name"] && t["type"].as_str().is_some_and(|k| k.starts_with("web_search"))
            });
            let choice = if searches {
                json!({ "type": "web_search" })
            } else {
                json!({ "type": "function", "name": tool_choice["name"] })
            };
            out.insert("tool_choice".into(), choice);
        }
        _ => {}
    }
    if tool_choice["disable_parallel_tool_use"].as_bool() == Some(true) {
        out.insert("parallel_tool_calls".into(), json!(false));
    }
    if let Some(reasoning) = reasoning(body) {
        out.insert("reasoning".into(), reasoning);
    }
    if body["speed"] == "fast" {
        out.insert("service_tier".into(), json!("priority"));
    }
    if let Some(max) = body["max_tokens"].as_i64() {
        out.insert("max_output_tokens".into(), json!(max));
    }
    let format = &body["output_config"]["format"];
    match format["type"].as_str() {
        None => {}
        Some("json_schema") => {
            out.insert(
                "text".into(),
                json!({ "format": { "type": "json_schema", "name": "output", "schema": format["schema"], "strict": true } }),
            );
        }
        Some(other) => {
            return Err(format!(
                "The output format `{other}` is not supported. Use `json_schema`."
            ));
        }
    }
    Ok(Value::Object(out))
}

/// Effort from `output_config.effort`, else from the thinking budget.
fn reasoning(body: &Value) -> Option<Value> {
    let thinking = &body["thinking"];
    let kind = thinking["type"].as_str();
    let effort = body["output_config"]["effort"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| match kind {
            Some("disabled") => Some("none".into()),
            Some("enabled") => {
                let budget = thinking["budget_tokens"].as_i64().unwrap_or(0);
                Some(
                    match budget {
                        b if b >= 32_000 => "high",
                        b if b >= 10_000 => "medium",
                        b if b >= 2_000 => "low",
                        _ => "minimal",
                    }
                    .into(),
                )
            }
            _ => None,
        });
    let summarize = matches!(kind, Some("enabled" | "adaptive")) && thinking["display"] != "omitted";
    if effort.is_none() && !summarize {
        return None;
    }
    let mut reasoning = Map::new();
    if let Some(effort) = effort {
        reasoning.insert("effort".into(), json!(effort));
    }
    if summarize {
        reasoning.insert("summary".into(), json!("auto"));
    }
    Some(Value::Object(reasoning))
}

fn tool(tool: &Value) -> Result<Value, String> {
    let kind = tool["type"].as_str().unwrap_or("custom");
    if kind.starts_with("web_search") {
        return web_search_tool(tool);
    }
    if kind != "custom" {
        return Err(format!(
            "The tool type `{kind}` works only with Anthropic models. Use a function tool with an input schema."
        ));
    }
    Ok(json!({
        "type": "function",
        "name": tool["name"],
        "description": tool["description"],
        "parameters": tool["input_schema"],
        "strict": tool["strict"].as_bool().unwrap_or(false),
    }))
}

/// The backend search can only be limited to domains; it cannot block domains. `max_uses` has
/// no backend equivalent, so it is not applied.
fn web_search_tool(tool: &Value) -> Result<Value, String> {
    if tool["blocked_domains"].as_array().is_some_and(|list| !list.is_empty()) {
        return Err("Web search with `blocked_domains` is not supported. Use `allowed_domains`.".into());
    }
    let mut out = json!({ "type": "web_search" });
    if let Some(domains) = tool["allowed_domains"].as_array().filter(|list| !list.is_empty()) {
        out["filters"] = json!({ "allowed_domains": domains });
    }
    if tool["user_location"].is_object() {
        let location = &tool["user_location"];
        out["user_location"] = json!({
            "type": "approximate",
            "city": location["city"],
            "region": location["region"],
            "country": location["country"],
            "timezone": location["timezone"],
        });
    }
    Ok(out)
}

/// A user message can hold tool results and new content. Tool results become their own items,
/// before the rest, in their order.
fn user_message(blocks: &[Value], input: &mut Vec<Value>) -> Result<(), String> {
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str().unwrap_or_default() {
            "tool_result" => {
                let mut output = tool_result_output(&block["content"])?;
                if block["is_error"].as_bool() == Some(true) {
                    output = match output {
                        Value::String(text) => json!(format!("Error: {text}")),
                        Value::Array(mut parts) => {
                            parts.insert(0, json!({ "type": "input_text", "text": "Error:" }));
                            Value::Array(parts)
                        }
                        other => other,
                    };
                }
                input
                    .push(json!({ "type": "function_call_output", "call_id": block["tool_use_id"], "output": output }));
            }
            _ => {
                if let Some(part) = content_part(block)? {
                    parts.push(part);
                }
            }
        }
    }
    if !parts.is_empty() {
        input.push(json!({ "type": "message", "role": "user", "content": parts }));
    }
    Ok(())
}

/// Tool result content: plain text, or a list of parts when it has images.
fn tool_result_output(content: &Value) -> Result<Value, String> {
    let blocks = match content {
        Value::String(text) => return Ok(json!(text)),
        Value::Array(blocks) => blocks,
        _ => return Ok(json!("")),
    };
    if blocks.iter().all(|b| b["type"] == "text") {
        let text: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
        return Ok(json!(text.join("\n")));
    }
    let parts: Vec<Value> = blocks
        .iter()
        .map(content_part)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(Value::Array(parts))
}

fn content_part(block: &Value) -> Result<Option<Value>, String> {
    let source = &block["source"];
    let data_url = || {
        format!(
            "data:{};base64,{}",
            source["media_type"].as_str().unwrap_or_default(),
            source["data"].as_str().unwrap_or_default()
        )
    };
    Ok(match block["type"].as_str().unwrap_or_default() {
        "text" => Some(json!({ "type": "input_text", "text": block["text"] })),
        "image" => match source["type"].as_str() {
            Some("base64") => Some(json!({ "type": "input_image", "image_url": data_url() })),
            Some("url") => Some(json!({ "type": "input_image", "image_url": source["url"] })),
            _ => return Err("Only base64 and URL images are supported.".into()),
        },
        "document" => match source["type"].as_str() {
            Some("base64") => Some(json!({
                "type": "input_file",
                "file_data": data_url(),
                "filename": block["title"].as_str().unwrap_or("document.pdf"),
            })),
            Some("text") => Some(json!({ "type": "input_text", "text": source["data"] })),
            Some("url") => Some(json!({ "type": "input_file", "file_url": source["url"] })),
            _ => return Err("This document source is not supported.".into()),
        },
        "search_result" => Some(json!({ "type": "input_text", "text": block.to_string() })),
        // Thinking in a user turn does not exist; other blocks carry no content for the model.
        _ => None,
    })
}

fn assistant_message(blocks: &[Value], input: &mut Vec<Value>) {
    let has_tool_use = blocks.iter().any(|b| b["type"] == "tool_use");
    for block in blocks {
        match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let text = block["text"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    let phase = if has_tool_use { "commentary" } else { "final_answer" };
                    input.push(json!({
                        "type": "message", "role": "assistant", "phase": phase,
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
            }
            "thinking" => {
                // Only reasoning that this gateway made can go back to the OpenAI model.
                if let Some(encrypted) = block["signature"]
                    .as_str()
                    .and_then(|s| s.strip_prefix(SIGNATURE_PREFIX))
                {
                    let text = block["thinking"].as_str().unwrap_or_default();
                    let summary = if text.is_empty() {
                        json!([])
                    } else {
                        json!([{ "type": "summary_text", "text": text }])
                    };
                    input.push(json!({ "type": "reasoning", "summary": summary, "encrypted_content": encrypted }));
                }
            }
            "tool_use" => input.push(json!({
                "type": "function_call",
                "call_id": block["id"],
                "name": block["name"],
                "arguments": block["input"].to_string(),
            })),
            // An earlier web search goes back as text: the backend cannot take search items
            // without stored state.
            "server_tool_use" if block["name"] == "web_search" => {
                let query = block["input"]["query"].as_str().unwrap_or_default();
                input.push(assistant_text(&format!("Web search: {query}")));
            }
            "web_search_tool_result" => {
                let sources: Vec<String> = block["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| {
                        r["url"]
                            .as_str()
                            .map(|url| format!("- {} {url}", r["title"].as_str().unwrap_or_default()))
                    })
                    .collect();
                if !sources.is_empty() {
                    input.push(assistant_text(&format!("Search results:\n{}", sources.join("\n"))));
                }
            }
            _ => {}
        }
    }
}

fn assistant_text(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
}

// ---------------------------------------------------------------------------------------------
// Response

fn usage(response: &Value) -> Value {
    let u = &response["usage"];
    let input = u["input_tokens"].as_i64().unwrap_or(0);
    let cached = u["input_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0);
    json!({
        // Anthropic counts cached input separately.
        "input_tokens": (input - cached).max(0),
        "cache_read_input_tokens": cached,
        "cache_creation_input_tokens": 0,
        "output_tokens": u["output_tokens"].as_i64().unwrap_or(0),
    })
}

/// A cut-off answer can contain an unfinished tool call, so the cut-off reason comes first.
fn stop_reason(response: &Value, has_tool_use: bool, refused: bool) -> &'static str {
    match response["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens") => "max_tokens",
        Some("content_filter") => "refusal",
        _ if refused => "refusal",
        _ if has_tool_use => "tool_use",
        _ => "end_turn",
    }
}

/// True when a message item of the response has a refusal part.
fn refused(response: &Value) -> bool {
    response["output"].as_array().into_iter().flatten().any(|item| {
        item["content"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|part| part["type"] == "refusal")
    })
}

fn thinking_block(item: &Value, show: bool) -> Option<Value> {
    let encrypted = item["encrypted_content"].as_str()?;
    let text: String = if show {
        item["summary"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        String::new()
    };
    Some(json!({ "type": "thinking", "thinking": text, "signature": format!("{SIGNATURE_PREFIX}{encrypted}") }))
}

/// A URL citation of a web search answer as an Anthropic citation. The cited text is known only
/// when the full text is.
fn citation(annotation: &Value, text: Option<&str>) -> Option<Value> {
    if annotation["type"] != "url_citation" {
        return None;
    }
    let cited = text
        .zip(annotation["start_index"].as_u64().zip(annotation["end_index"].as_u64()))
        .map(|(text, (start, end))| {
            text.chars()
                .skip(start as usize)
                .take(end.saturating_sub(start) as usize)
                .collect::<String>()
        })
        .unwrap_or_default();
    Some(json!({
        "type": "web_search_result_location",
        "url": annotation["url"],
        "title": annotation["title"],
        "cited_text": cited,
        "encrypted_index": "",
    }))
}

/// Web search as the Anthropic server tool blocks.
fn web_search_blocks(item: &Value) -> [Value; 2] {
    let id = format!("srvtoolu_{}", item["id"].as_str().unwrap_or_default());
    let results: Vec<Value> = item["action"]["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            // Anthropic needs a title; a source can come without one.
            let title = s["title"].as_str().or_else(|| s["url"].as_str()).unwrap_or_default();
            json!({ "type": "web_search_result", "url": s["url"], "title": title, "encrypted_content": "", "page_age": null })
        })
        .collect();
    [
        json!({ "type": "server_tool_use", "id": id, "name": "web_search", "input": { "query": item["action"]["query"] } }),
        json!({ "type": "web_search_tool_result", "tool_use_id": id, "content": results }),
    ]
}

pub fn from_response(response: &Value, model: &str, show_thinking: bool) -> Value {
    let mut content = Vec::new();
    let mut has_tool_use = false;
    for item in response["output"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or_default() {
            "reasoning" => content.extend(thinking_block(item, show_thinking)),
            "message" => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if let Some(text) = part["text"].as_str().or_else(|| part["refusal"].as_str()) {
                        let citations: Vec<Value> = part["annotations"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|a| citation(a, Some(text)))
                            .collect();
                        let mut block = json!({ "type": "text", "text": text });
                        if !citations.is_empty() {
                            block["citations"] = Value::Array(citations);
                        }
                        content.push(block);
                    }
                }
            }
            "function_call" => {
                has_tool_use = true;
                let input: Value =
                    serde_json::from_str(item["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                content
                    .push(json!({ "type": "tool_use", "id": item["call_id"], "name": item["name"], "input": input }));
            }
            "web_search_call" => content.extend(web_search_blocks(item)),
            _ => {}
        }
    }
    json!({
        "id": format!("msg_{}", response["id"].as_str().unwrap_or_default()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason(response, has_tool_use, refused(response)),
        "stop_sequence": null,
        "usage": usage(response),
    })
}

async fn collect(mut rx: tokio::sync::mpsc::Receiver<Msg>, model: &str, show_thinking: bool) -> Response {
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Done(response) => return Json(from_response(&response, model, show_thinking)).into_response(),
            Msg::Failed(failure) => return error(failure),
            Msg::Event(_) => {}
        }
    }
    error(Failure::new(
        StatusCode::BAD_GATEWAY,
        "upstream_closed",
        "The request ended without an answer.",
    ))
}

/// Turns Responses events into Anthropic stream events.
struct EventEncoder {
    model: String,
    show_thinking: bool,
    started: bool,
    next_index: usize,
    /// Output item id → open content block index.
    open: HashMap<String, usize>,
    has_tool_use: bool,
}

impl EventEncoder {
    fn new(model: String, show_thinking: bool) -> Self {
        Self {
            model,
            show_thinking,
            started: false,
            next_index: 0,
            open: HashMap::new(),
            has_tool_use: false,
        }
    }

    fn event(kind: &str, data: Value) -> Bytes {
        sse::frame(Some(kind), &data.to_string())
    }

    fn start_block(&mut self, out: &mut Vec<Bytes>, item_id: &str, block: Value) {
        let index = self.next_index;
        self.next_index += 1;
        self.open.insert(item_id.to_owned(), index);
        out.push(Self::event(
            "content_block_start",
            json!({ "type": "content_block_start", "index": index, "content_block": block }),
        ));
    }

    fn delta(out: &mut Vec<Bytes>, index: usize, delta: Value) {
        out.push(Self::event(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": index, "delta": delta }),
        ));
    }

    fn stop_block(&mut self, out: &mut Vec<Bytes>, item_id: &str) {
        if let Some(index) = self.open.remove(item_id) {
            out.push(Self::event(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": index }),
            ));
        }
    }

    /// A whole block at once (start, one delta, stop).
    fn complete_block(&mut self, out: &mut Vec<Bytes>, block: Value) {
        let index = self.next_index;
        self.next_index += 1;
        out.push(Self::event(
            "content_block_start",
            json!({ "type": "content_block_start", "index": index, "content_block": block }),
        ));
        out.push(Self::event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": index }),
        ));
    }

    fn encode(&mut self, msg: Msg) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(Self::event(
                "message_start",
                json!({ "type": "message_start", "message": {
                    "id": format!("msg_{}", crate::crypto::random_token(12)), "type": "message", "role": "assistant",
                    "model": self.model, "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                } }),
            ));
        }
        match msg {
            Msg::Event(event) => self.encode_event(&event.kind, &event.data, &mut out),
            Msg::Done(response) => {
                for id in self.open.keys().cloned().collect::<Vec<_>>() {
                    self.stop_block(&mut out, &id);
                }
                let usage = usage(&response);
                out.push(Self::event(
                    "message_delta",
                    json!({ "type": "message_delta",
                            "delta": { "stop_reason": stop_reason(&response, self.has_tool_use, refused(&response)), "stop_sequence": null },
                            "usage": usage }),
                ));
                out.push(Self::event("message_stop", json!({ "type": "message_stop" })));
            }
            Msg::Failed(failure) => {
                let kind = match failure.status.as_u16() {
                    429 => "rate_limit_error",
                    503 | 529 => "overloaded_error",
                    _ => "api_error",
                };
                out.push(Self::event(
                    "error",
                    json!({ "type": "error", "error": { "type": kind, "message": failure.message } }),
                ));
            }
        }
        out
    }

    fn encode_event(&mut self, kind: &str, data: &Value, out: &mut Vec<Bytes>) {
        let item = &data["item"];
        let item_id = data["item_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .unwrap_or_default()
            .to_owned();
        match kind {
            "response.output_item.added" => match item["type"].as_str().unwrap_or_default() {
                "message" => self.start_block(out, &item_id, json!({ "type": "text", "text": "" })),
                "reasoning" => self.start_block(out, &item_id, json!({ "type": "thinking", "thinking": "" })),
                "function_call" => {
                    self.has_tool_use = true;
                    self.start_block(
                        out,
                        &item_id,
                        json!({ "type": "tool_use", "id": item["call_id"], "name": item["name"], "input": {} }),
                    );
                }
                _ => {}
            },
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(index) = self.open.get(&item_id).copied() {
                    Self::delta(out, index, json!({ "type": "text_delta", "text": data["delta"] }));
                }
            }
            "response.output_text.annotation.added" => {
                if let (Some(index), Some(citation)) =
                    (self.open.get(&item_id).copied(), citation(&data["annotation"], None))
                {
                    Self::delta(out, index, json!({ "type": "citations_delta", "citation": citation }));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(index) = self.open.get(&item_id).copied().filter(|_| self.show_thinking) {
                    Self::delta(
                        out,
                        index,
                        json!({ "type": "thinking_delta", "thinking": data["delta"] }),
                    );
                }
            }
            "response.reasoning_summary_part.done" => {
                if let Some(index) = self.open.get(&item_id).copied().filter(|_| self.show_thinking) {
                    Self::delta(out, index, json!({ "type": "thinking_delta", "thinking": "\n\n" }));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(index) = self.open.get(&item_id).copied() {
                    Self::delta(
                        out,
                        index,
                        json!({ "type": "input_json_delta", "partial_json": data["delta"] }),
                    );
                }
            }
            "response.output_item.done" => match item["type"].as_str().unwrap_or_default() {
                "reasoning" => {
                    if let (Some(index), Some(encrypted)) =
                        (self.open.get(&item_id).copied(), item["encrypted_content"].as_str())
                    {
                        let signature = format!("{SIGNATURE_PREFIX}{encrypted}");
                        Self::delta(out, index, json!({ "type": "signature_delta", "signature": signature }));
                    }
                    self.stop_block(out, &item_id);
                }
                "web_search_call" => {
                    for block in web_search_blocks(item) {
                        self.complete_block(out, block);
                    }
                }
                _ => self.stop_block(out, &item_id),
            },
            _ => {}
        }
    }
}
