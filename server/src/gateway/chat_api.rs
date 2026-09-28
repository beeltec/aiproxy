//! `POST /v1/chat/completions`: Chat Completions clients, translated to and from Responses.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::auth::{Admission, ApiKey};
use super::engine::{self, Failure, Msg};
use super::responses_api::error;
use super::{request, sse};
use crate::db::now;
use crate::state::AppState;

pub async fn create(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    Extension(admission): Extension<Admission>,
    Json(body): Json<Value>,
) -> Response {
    let converted = match to_responses(&body) {
        Ok(converted) => converted,
        Err(message) => return error(Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message)),
    };
    let requested = body["model"].as_str().unwrap_or_default().to_owned();
    let include_usage = body["stream_options"]["include_usage"].as_bool() == Some(true);
    let prepared = match request::prepare(&state, &key, converted, "chat", "chat", None, admission).await {
        Ok(prepared) => prepared,
        Err(failure) => return error(failure),
    };
    let stream = prepared.job.stream;
    let rx = match engine::start(&state, prepared.job).await {
        Ok(rx) => rx,
        Err(failure) => return error(failure),
    };
    if stream {
        let mut encoder = ChunkEncoder::new(requested, include_usage);
        return sse::response(rx, move |msg| encoder.encode(msg), ": ping\n\n");
    }
    collect(rx, &requested).await
}

// ---------------------------------------------------------------------------------------------
// Request

/// Converts a Chat Completions body to a Responses body.
pub fn to_responses(body: &Value) -> Result<Value, String> {
    let messages = body["messages"].as_array().ok_or("The field `messages` is missing.")?;
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for message in messages {
        match message["role"].as_str().unwrap_or_default() {
            "system" | "developer" => instructions.push(text_of(&message["content"])),
            "user" => {
                input.push(json!({ "type": "message", "role": "user", "content": user_parts(&message["content"])? }))
            }
            "assistant" => {
                let text = text_of(&message["content"]);
                let tool_calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
                if !text.is_empty() {
                    // Text before tool calls is an intermediate update, not the final answer.
                    let phase = if tool_calls.is_empty() {
                        "final_answer"
                    } else {
                        "commentary"
                    };
                    input.push(json!({
                        "type": "message", "role": "assistant", "phase": phase,
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call["id"],
                        "name": call["function"]["name"],
                        "arguments": call["function"]["arguments"].as_str().unwrap_or("{}"),
                    }));
                }
            }
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": message["tool_call_id"],
                "output": text_of(&message["content"]),
            })),
            other => return Err(format!("Unknown message role `{other}`.")),
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), body["model"].clone());
    out.insert("input".into(), Value::Array(input));
    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    out.insert("stream".into(), json!(body["stream"].as_bool().unwrap_or(false)));
    if let Some(tools) = body["tools"].as_array() {
        let tools: Vec<Value> = tools
            .iter()
            .filter(|t| t["type"] == "function")
            .map(|t| {
                let f = &t["function"];
                json!({ "type": "function", "name": f["name"], "description": f["description"],
                        "parameters": f["parameters"], "strict": f["strict"].as_bool().unwrap_or(false) })
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
    }
    if !body["web_search_options"].is_null() {
        let tools = out.entry("tools").or_insert_with(|| json!([]));
        if let Some(list) = tools.as_array_mut() {
            list.push(json!({ "type": "web_search" }));
        }
    }
    match &body["tool_choice"] {
        Value::String(choice) => {
            out.insert("tool_choice".into(), json!(choice));
        }
        Value::Object(choice) if choice.get("type").and_then(Value::as_str) == Some("function") => {
            out.insert(
                "tool_choice".into(),
                json!({ "type": "function", "name": choice["function"]["name"] }),
            );
        }
        _ => {}
    }
    if let Some(parallel) = body["parallel_tool_calls"].as_bool() {
        out.insert("parallel_tool_calls".into(), json!(parallel));
    }
    let effort = body["reasoning_effort"]
        .as_str()
        .or_else(|| body["reasoning"]["effort"].as_str());
    if let Some(effort) = effort {
        out.insert("reasoning".into(), json!({ "effort": effort, "summary": "auto" }));
    }
    if let Some(tier) = body["service_tier"].as_str() {
        out.insert("service_tier".into(), json!(tier));
    }
    if let Some(max) = body["max_completion_tokens"]
        .as_i64()
        .or_else(|| body["max_tokens"].as_i64())
    {
        out.insert("max_output_tokens".into(), json!(max));
    }
    match body["response_format"]["type"].as_str() {
        Some("json_object") => {
            out.insert("text".into(), json!({ "format": { "type": "json_object" } }));
        }
        Some("json_schema") => {
            let schema = &body["response_format"]["json_schema"];
            out.insert(
                "text".into(),
                json!({ "format": { "type": "json_schema", "name": schema["name"], "schema": schema["schema"],
                                    "strict": schema["strict"].as_bool().unwrap_or(false) } }),
            );
        }
        _ => {}
    }
    Ok(Value::Object(out))
}

/// Text of a message content (a string or a list of text parts).
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn user_parts(content: &Value) -> Result<Vec<Value>, String> {
    let parts = match content {
        Value::String(text) => return Ok(vec![json!({ "type": "input_text", "text": text })]),
        Value::Array(parts) => parts,
        _ => return Ok(Vec::new()),
    };
    parts
        .iter()
        .map(|part| match part["type"].as_str().unwrap_or_default() {
            "text" => Ok(json!({ "type": "input_text", "text": part["text"] })),
            "image_url" => {
                let image = &part["image_url"];
                let url = image.as_str().or_else(|| image["url"].as_str()).unwrap_or_default();
                let mut out = json!({ "type": "input_image", "image_url": url });
                if let Some(detail) = image["detail"].as_str() {
                    out["detail"] = json!(detail);
                }
                Ok(out)
            }
            "input_audio" => Ok(json!({ "type": "input_audio", "input_audio": part["input_audio"] })),
            "file" => {
                let file = &part["file"];
                if !file["file_id"].is_null() {
                    return Err("References to stored files are not supported. Send `file_data` inline.".into());
                }
                Ok(json!({ "type": "input_file", "file_data": file["file_data"], "filename": file["filename"] }))
            }
            other => Err(format!("Unknown content part `{other}`.")),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Response

fn usage(response: &Value) -> Value {
    let u = &response["usage"];
    let input = u["input_tokens"].as_i64().unwrap_or(0);
    let output = u["output_tokens"].as_i64().unwrap_or(0);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": { "cached_tokens": u["input_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0) },
        "completion_tokens_details": { "reasoning_tokens": u["output_tokens_details"]["reasoning_tokens"].as_i64().unwrap_or(0) },
    })
}

/// A cut-off answer can contain unfinished tool calls, so the cut-off reason comes first.
fn finish_reason(response: &Value, has_tool_calls: bool) -> &'static str {
    match response["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ if has_tool_calls => "tool_calls",
        _ => "stop",
    }
}

/// Builds the chat completion from the final Responses object.
pub fn from_response(response: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut refusal = String::new();
    let mut annotations = Vec::new();
    let mut tool_calls = Vec::new();
    for item in response["output"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or_default() {
            "message" => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if let Some(t) = part["text"].as_str() {
                        // Chat indexes count from the start of the full message text.
                        let offset = text.chars().count() as u64;
                        for a in part["annotations"].as_array().into_iter().flatten() {
                            if a["type"] == "url_citation" {
                                let index = |field: &str| a[field].as_u64().map(|i| i + offset);
                                annotations.push(json!({ "type": "url_citation", "url_citation": {
                                    "url": a["url"], "title": a["title"],
                                    "start_index": index("start_index"), "end_index": index("end_index"),
                                } }));
                            }
                        }
                        text.push_str(t);
                    }
                    if let Some(t) = part["refusal"].as_str() {
                        refusal.push_str(t);
                    }
                }
            }
            "reasoning" => {
                for part in item["summary"].as_array().into_iter().flatten() {
                    if let Some(t) = part["text"].as_str() {
                        reasoning.push_str(t);
                    }
                }
            }
            "function_call" => tool_calls.push(json!({
                "id": item["call_id"], "type": "function",
                "function": { "name": item["name"], "arguments": item["arguments"] },
            })),
            _ => {}
        }
    }
    let mut message =
        json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !refusal.is_empty() {
        message["refusal"] = json!(refusal);
    }
    if !annotations.is_empty() {
        message["annotations"] = Value::Array(annotations);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls.clone());
    }
    json!({
        "id": format!("chatcmpl-{}", response["id"].as_str().unwrap_or_default()),
        "object": "chat.completion",
        "created": response["created_at"].as_i64().unwrap_or_else(now),
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason(response, !tool_calls.is_empty()) }],
        "usage": usage(response),
        "service_tier": response["service_tier"],
    })
}

async fn collect(mut rx: tokio::sync::mpsc::Receiver<Msg>, model: &str) -> Response {
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Done(response) => return Json(from_response(&response, model)).into_response(),
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

/// Turns Responses events into chat completion chunks.
struct ChunkEncoder {
    model: String,
    include_usage: bool,
    id: String,
    created: i64,
    started: bool,
    /// Output item id → tool call index.
    tool_index: HashMap<String, usize>,
}

impl ChunkEncoder {
    fn new(model: String, include_usage: bool) -> Self {
        Self {
            model,
            include_usage,
            id: format!("chatcmpl-{}", crate::crypto::random_token(12)),
            created: now(),
            started: false,
            tool_index: HashMap::new(),
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Bytes {
        let data = json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        sse::frame(None, &data.to_string())
    }

    fn encode(&mut self, msg: Msg) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
        match msg {
            Msg::Event(event) => {
                let data = &event.data;
                match event.kind.as_str() {
                    "response.output_text.delta" => out.push(self.chunk(json!({ "content": data["delta"] }), None)),
                    "response.refusal.delta" => out.push(self.chunk(json!({ "refusal": data["delta"] }), None)),
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        out.push(self.chunk(json!({ "reasoning_content": data["delta"] }), None));
                    }
                    "response.output_item.added" if data["item"]["type"] == "function_call" => {
                        let index = self.tool_index.len();
                        self.tool_index
                            .insert(data["item"]["id"].as_str().unwrap_or_default().to_owned(), index);
                        out.push(self.chunk(
                            json!({ "tool_calls": [{ "index": index, "id": data["item"]["call_id"], "type": "function",
                                                     "function": { "name": data["item"]["name"], "arguments": "" } }] }),
                            None,
                        ));
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(index) = self.tool_index.get(data["item_id"].as_str().unwrap_or_default()) {
                            out.push(self.chunk(
                                json!({ "tool_calls": [{ "index": index, "function": { "arguments": data["delta"] } }] }),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Msg::Done(response) => {
                let finish = finish_reason(&response, !self.tool_index.is_empty());
                out.push(self.chunk(json!({}), Some(finish)));
                if self.include_usage {
                    let data = json!({
                        "id": self.id, "object": "chat.completion.chunk", "created": self.created,
                        "model": self.model, "choices": [], "usage": usage(&response),
                    });
                    out.push(sse::frame(None, &data.to_string()));
                }
                out.push(sse::frame(None, "[DONE]"));
            }
            Msg::Failed(failure) => {
                let data =
                    json!({ "error": { "message": failure.message, "type": "server_error", "code": failure.code } });
                out.push(sse::frame(None, &data.to_string()));
                out.push(sse::frame(None, "[DONE]"));
            }
        }
        out
    }
}
