//! Chat Completions upstreams (OpenAI chat-only models, OpenRouter): the Responses request as a
//! Chat request, and the Chat stream as Responses events.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Map, Value, json};

use super::engine::{Event, Failure};
use super::provider::Decoder;
use super::routing::Route;
use crate::connections::Kind;
use crate::crypto::random_token;
use crate::db::now;
use crate::usage::Tokens;

/// OpenRouter reasoning details go to clients inside encrypted reasoning with this prefix.
const OPENROUTER_PREFIX: &str = "aipo1:";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Chat usage in billing categories. OpenRouter reports generated image tokens in the output
/// details; they are part of the output total.
pub fn tokens(usage: &Value) -> Tokens {
    let mut tokens = Tokens::from_openai(usage);
    let images = usage["completion_tokens_details"]["image_tokens"]
        .as_i64()
        .unwrap_or(0)
        .max(0);
    let images = images.min(tokens.output_text);
    tokens.output_image = images;
    tokens.output_text -= images;
    tokens
}

// ---------------------------------------------------------------------------------------------
// Request

/// One Responses content part as a Chat content part.
/// OpenRouter also takes a document URL in `file_data`; OpenAI takes only inline data.
fn chat_part(part: &Value, kind: Kind) -> Result<Option<Value>, String> {
    Ok(match part["type"].as_str().unwrap_or_default() {
        "input_text" | "output_text" | "text" => Some(json!({ "type": "text", "text": part["text"] })),
        "input_image" => {
            let mut image = json!({ "url": part["image_url"] });
            if let Some(detail) = part["detail"].as_str() {
                image["detail"] = json!(detail);
            }
            Some(json!({ "type": "image_url", "image_url": image }))
        }
        "input_file" => {
            let url = part["file_url"].as_str().filter(|_| kind == Kind::OpenRouter);
            let Some(data) = part["file_data"].as_str().or(url) else {
                return Err("This model takes files only as inline `file_data`.".into());
            };
            Some(json!({ "type": "file", "file": { "file_data": data, "filename": part["filename"] } }))
        }
        "input_audio" => Some(json!({ "type": "input_audio", "input_audio": part["input_audio"] })),
        "input_video" => Some(json!({ "type": "video_url", "video_url": { "url": part["video_url"] } })),
        "refusal" => None,
        other => return Err(format!("The content type `{other}` is not supported for this model.")),
    })
}

fn text_of(parts: &Value) -> String {
    match parts {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Converts a Responses body to a Chat body. Tool results with images: Chat tool messages take
/// only text, so the images follow in a user message.
pub fn encode(body: &Value, route: &Route, kind: Kind) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instructions) = body["instructions"].as_str().filter(|i| !i.is_empty()) {
        messages.push(json!({ "role": "system", "content": instructions }));
    }
    let items = match &body["input"] {
        Value::String(text) => vec![json!({ "type": "message", "role": "user", "content": text })],
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    };
    // OpenRouter reasoning waits for the assistant message that follows it.
    let mut reasoning_details: Vec<Value> = Vec::new();
    // Images of tool results wait until all tool messages of the group are written: Chat
    // needs a tool message for every call before the next user message.
    let mut tool_images: Vec<Value> = Vec::new();
    for item in &items {
        if item["type"] != "function_call_output" && !tool_images.is_empty() {
            messages.push(json!({ "role": "user", "content": std::mem::take(&mut tool_images) }));
        }
        match item["type"].as_str().unwrap_or("message") {
            "message" => {
                let role = item["role"].as_str().unwrap_or("user");
                match role {
                    "assistant" => {
                        let mut message = json!({ "role": "assistant", "content": text_of(&item["content"]) });
                        let refusal: String = item["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p["refusal"].as_str())
                            .collect();
                        if !refusal.is_empty() {
                            message["refusal"] = json!(refusal);
                        }
                        if !reasoning_details.is_empty() {
                            message["reasoning_details"] = Value::Array(std::mem::take(&mut reasoning_details));
                        }
                        messages.push(message);
                    }
                    "system" | "developer" => {
                        messages.push(json!({ "role": role, "content": text_of(&item["content"]) }));
                    }
                    _ => {
                        let content = match &item["content"] {
                            Value::String(text) => json!(text),
                            Value::Array(parts) => {
                                let mut out = Vec::new();
                                for part in parts {
                                    out.extend(chat_part(part, kind)?);
                                }
                                Value::Array(out)
                            }
                            _ => json!(""),
                        };
                        messages.push(json!({ "role": "user", "content": content }));
                    }
                }
            }
            "function_call" => {
                let call = json!({ "id": item["call_id"], "type": "function",
                                   "function": { "name": item["name"], "arguments": item["arguments"] } });
                match messages.last_mut().filter(|m| m["role"] == "assistant") {
                    Some(message) => {
                        if !message["tool_calls"].is_array() {
                            message["tool_calls"] = json!([]);
                        }
                        if !reasoning_details.is_empty() {
                            let mut details = message["reasoning_details"].as_array().cloned().unwrap_or_default();
                            details.append(&mut reasoning_details);
                            message["reasoning_details"] = Value::Array(details);
                        }
                        message["tool_calls"].as_array_mut().expect("a list").push(call);
                    }
                    None => {
                        let mut message = json!({ "role": "assistant", "content": null, "tool_calls": [call] });
                        if !reasoning_details.is_empty() {
                            message["reasoning_details"] = Value::Array(std::mem::take(&mut reasoning_details));
                        }
                        messages.push(message);
                    }
                }
            }
            "function_call_output" => {
                let output = &item["output"];
                messages.push(json!({ "role": "tool", "tool_call_id": item["call_id"], "content": text_of(output) }));
                let mut images: Vec<Value> = Vec::new();
                for part in output.as_array().into_iter().flatten() {
                    if matches!(part["type"].as_str(), Some("input_image" | "input_file")) {
                        images.extend(chat_part(part, kind)?);
                    }
                }
                if !images.is_empty() {
                    tool_images.push(json!({ "type": "text", "text": format!(
                        "Images and files from the result of tool call {}:",
                        item["call_id"].as_str().unwrap_or_default()
                    ) }));
                    tool_images.extend(images);
                }
            }
            "reasoning" => {
                if kind == Kind::OpenRouter
                    && let Some(details) = item["encrypted_content"]
                        .as_str()
                        .and_then(|e| e.strip_prefix(OPENROUTER_PREFIX))
                        .and_then(|e| b64().decode(e).ok())
                        .and_then(|bytes| serde_json::from_slice::<Vec<Value>>(&bytes).ok())
                {
                    reasoning_details.extend(details);
                }
            }
            // Earlier searches have no Chat form; the answer text stays in the history.
            "web_search_call" => {}
            other => return Err(format!("The input item `{other}` is not supported for this model.")),
        }
    }

    if !tool_images.is_empty() {
        messages.push(json!({ "role": "user", "content": tool_images }));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(route.upstream_model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(true));
    out.insert("stream_options".into(), json!({ "include_usage": true }));

    let mut tools = Vec::new();
    for tool in body["tools"].as_array().into_iter().flatten() {
        match tool["type"].as_str().unwrap_or_default() {
            "function" => tools.push(json!({ "type": "function", "function": {
                "name": tool["name"], "description": tool["description"], "parameters": tool["parameters"],
                "strict": tool["strict"].as_bool().unwrap_or(false),
            } })),
            "web_search" => match kind {
                // Tools switched off: the plugin would search anyway.
                _ if body["tool_choice"] == "none" => {}
                Kind::OpenRouter => {
                    let mut plugin = json!({ "id": "web" });
                    if let Some(domains) = tool["filters"]["allowed_domains"].as_array() {
                        plugin["include_domains"] = json!(domains);
                    }
                    out.insert("plugins".into(), json!([plugin]));
                }
                _ => {
                    if !tool["filters"]["allowed_domains"].is_null() {
                        return Err("Web search with `allowed_domains` is not supported for this model.".into());
                    }
                    let mut options = json!({});
                    if let Some(size) = tool["search_context_size"].as_str() {
                        options["search_context_size"] = json!(size);
                    }
                    if tool["user_location"].is_object() {
                        let mut location = tool["user_location"].clone();
                        if let Some(map) = location.as_object_mut() {
                            map.remove("type");
                        }
                        options["user_location"] = json!({ "type": "approximate", "approximate": location });
                    }
                    out.insert("web_search_options".into(), options);
                }
            },
            other => return Err(format!("The tool type `{other}` is not supported for this model.")),
        }
    }
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        match &body["tool_choice"] {
            Value::String(choice) => {
                out.insert("tool_choice".into(), json!(choice));
            }
            Value::Object(choice) if choice.get("type").and_then(Value::as_str) == Some("function") => {
                out.insert(
                    "tool_choice".into(),
                    json!({ "type": "function", "function": { "name": choice["name"] } }),
                );
            }
            _ => {}
        }
        if let Some(parallel) = body["parallel_tool_calls"].as_bool() {
            out.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }
    if let Some(max) = body["max_output_tokens"].as_i64() {
        out.insert("max_completion_tokens".into(), json!(max));
    }
    for field in ["temperature", "top_p", "stop"] {
        if !body[field].is_null() {
            out.insert(field.into(), body[field].clone());
        }
    }
    if let Some(effort) = body["reasoning"]["effort"].as_str() {
        match kind {
            Kind::OpenRouter => {
                out.insert("reasoning".into(), json!({ "effort": effort }));
            }
            _ => {
                out.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }
    if kind == Kind::OpenAi && matches!(body["service_tier"].as_str(), Some("priority" | "fast" | "flex")) {
        let tier = if body["service_tier"] == "fast" {
            "priority"
        } else {
            body["service_tier"].as_str().unwrap_or_default()
        };
        out.insert("service_tier".into(), json!(tier));
    }
    let format = &body["text"]["format"];
    match format["type"].as_str() {
        Some("json_schema") => {
            out.insert(
                "response_format".into(),
                json!({ "type": "json_schema", "json_schema": {
                    "name": format["name"].as_str().unwrap_or("output"), "schema": format["schema"],
                    "strict": format["strict"].as_bool().unwrap_or(true),
                } }),
            );
        }
        Some("json_object") => {
            out.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        _ => {}
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------------------------
// Stream

/// The open message item: its text and refusal parts in the order they started.
#[derive(Default)]
struct Message {
    item_id: String,
    index: usize,
    text: String,
    refusal: String,
    text_part: Option<usize>,
    refusal_part: Option<usize>,
    parts: usize,
}

struct ToolCall {
    item_id: String,
    output_index: usize,
    call_id: String,
    name: String,
    arguments: String,
    /// `output_item.added` was sent (with the complete name).
    announced: bool,
}

/// Chat chunks as Responses events. Order: reasoning, then text, then tool calls.
pub struct ChatDecoder {
    kind: Kind,
    id: String,
    created: i64,
    model: String,
    started: bool,
    sequence: u64,
    next_index: usize,
    reasoning: Option<(String, usize, String)>,
    /// OpenRouter reasoning details by index, merged from the deltas.
    details: BTreeMap<i64, Value>,
    message: Option<Message>,
    annotations: Vec<Value>,
    tools: BTreeMap<i64, ToolCall>,
    done_items: Vec<(usize, Value)>,
    finish_reason: Option<String>,
    usage: Option<Tokens>,
    service_tier: Option<String>,
}

impl ChatDecoder {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            id: format!("resp_{}", random_token(12)),
            created: now(),
            model: String::new(),
            started: false,
            sequence: 0,
            next_index: 0,
            reasoning: None,
            details: BTreeMap::new(),
            message: None,
            annotations: Vec::new(),
            tools: BTreeMap::new(),
            done_items: Vec::new(),
            finish_reason: None,
            usage: None,
            service_tier: None,
        }
    }

    fn emit(&mut self, out: &mut Vec<Event>, kind: &str, mut data: Value) {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        out.push(Event {
            kind: kind.to_owned(),
            data,
        });
    }

    fn response(&self, status: &str) -> Value {
        json!({ "id": self.id, "object": "response", "created_at": self.created, "model": self.model,
                "status": status, "output": [] })
    }

    fn close_reasoning(&mut self, out: &mut Vec<Event>) {
        let Some((item_id, index, text)) = self.reasoning.take() else {
            return;
        };
        let summary = if text.is_empty() {
            json!([])
        } else {
            json!([{ "type": "summary_text", "text": text }])
        };
        let mut item = json!({ "id": item_id, "type": "reasoning", "summary": summary });
        if self.kind == Kind::OpenRouter && !self.details.is_empty() {
            let details: Vec<Value> = std::mem::take(&mut self.details).into_values().collect();
            let encoded = b64().encode(serde_json::to_vec(&details).unwrap_or_default());
            item["encrypted_content"] = json!(format!("{OPENROUTER_PREFIX}{encoded}"));
        }
        self.emit(
            out,
            "response.reasoning_summary_text.done",
            json!({ "item_id": item_id, "output_index": index, "summary_index": 0, "text": text }),
        );
        self.emit(
            out,
            "response.output_item.done",
            json!({ "output_index": index, "item": item.clone() }),
        );
        self.done_items.push((index, item));
    }

    fn close_message(&mut self, out: &mut Vec<Event>) {
        let Some(message) = self.message.take() else {
            return;
        };
        let (item_id, index) = (message.item_id, message.index);
        let mut content = vec![Value::Null; message.parts];
        if let Some(part_index) = message.text_part {
            let part = json!({ "type": "output_text", "text": message.text,
                               "annotations": std::mem::take(&mut self.annotations) });
            self.emit(
                out,
                "response.output_text.done",
                json!({ "item_id": item_id, "output_index": index,
                "content_index": part_index, "text": message.text }),
            );
            self.emit(
                out,
                "response.content_part.done",
                json!({ "item_id": item_id, "output_index": index,
                "content_index": part_index, "part": part.clone() }),
            );
            content[part_index] = part;
        }
        if let Some(part_index) = message.refusal_part {
            let part = json!({ "type": "refusal", "refusal": message.refusal });
            self.emit(
                out,
                "response.refusal.done",
                json!({ "item_id": item_id, "output_index": index,
                "content_index": part_index, "refusal": message.refusal }),
            );
            self.emit(
                out,
                "response.content_part.done",
                json!({ "item_id": item_id, "output_index": index,
                "content_index": part_index, "part": part.clone() }),
            );
            content[part_index] = part;
        }
        let item =
            json!({ "id": item_id, "type": "message", "role": "assistant", "status": "completed", "content": content });
        self.emit(
            out,
            "response.output_item.done",
            json!({ "output_index": index, "item": item.clone() }),
        );
        self.done_items.push((index, item));
    }

    /// Opens the message item and the text or refusal part in it. Returns the item id, the
    /// output index and the content index of the part.
    fn open_part(&mut self, out: &mut Vec<Event>, refusal: bool) -> (String, usize, usize) {
        if self.message.is_none() {
            self.close_reasoning(out);
            let item_id = format!("msg_{}", random_token(12));
            let index = self.next_index;
            self.next_index += 1;
            self.emit(
                out,
                "response.output_item.added",
                json!({ "output_index": index, "item": {
                "id": item_id, "type": "message", "role": "assistant", "status": "in_progress", "content": [] } }),
            );
            self.message = Some(Message {
                item_id,
                index,
                ..Message::default()
            });
        }
        let message = self.message.as_mut().expect("open");
        let slot = if refusal {
            &mut message.refusal_part
        } else {
            &mut message.text_part
        };
        let (item_id, index) = (message.item_id.clone(), message.index);
        let part_index = match *slot {
            Some(part_index) => part_index,
            None => {
                let part_index = message.parts;
                *slot = Some(part_index);
                message.parts += 1;
                let part = if refusal {
                    json!({ "type": "refusal", "refusal": "" })
                } else {
                    json!({ "type": "output_text", "text": "", "annotations": [] })
                };
                self.emit(
                    out,
                    "response.content_part.added",
                    json!({ "item_id": item_id, "output_index": index,
                    "content_index": part_index, "part": part }),
                );
                part_index
            }
        };
        (item_id, index, part_index)
    }

    fn open_reasoning(&mut self, out: &mut Vec<Event>) {
        if self.reasoning.is_none() {
            let item_id = format!("rs_{}", random_token(12));
            let index = self.next_index;
            self.next_index += 1;
            self.emit(
                out,
                "response.output_item.added",
                json!({ "output_index": index, "item": {
                "id": item_id, "type": "reasoning", "summary": [] } }),
            );
            self.emit(
                out,
                "response.reasoning_summary_part.added",
                json!({ "item_id": item_id, "output_index": index,
                "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
            );
            self.reasoning = Some((item_id, index, String::new()));
        }
    }

    fn reasoning_delta(&mut self, out: &mut Vec<Event>, delta: &str) {
        self.open_reasoning(out);
        let (item_id, index, text) = self.reasoning.as_mut().expect("open");
        text.push_str(delta);
        let (item_id, index) = (item_id.clone(), *index);
        self.emit(
            out,
            "response.reasoning_summary_text.delta",
            json!({ "item_id": item_id, "output_index": index, "summary_index": 0, "delta": delta }),
        );
    }

    fn merge_details(&mut self, details: &Value) {
        for detail in details.as_array().into_iter().flatten() {
            let index = detail["index"].as_i64().unwrap_or(self.details.len() as i64);
            let entry = self.details.entry(index).or_insert_with(|| json!({}));
            for (key, value) in detail.as_object().into_iter().flatten() {
                // Text parts arrive in pieces; they are appended in place.
                match (&mut entry[key], value.as_str()) {
                    (Value::String(text), Some(piece)) if matches!(key.as_str(), "text" | "summary" | "data") => {
                        text.push_str(piece);
                    }
                    (slot, _) => *slot = value.clone(),
                }
            }
        }
    }

    fn announce(&mut self, out: &mut Vec<Event>, index: i64) {
        let Some(tool) = self.tools.get_mut(&index).filter(|t| !t.announced) else {
            return;
        };
        tool.announced = true;
        let data = json!({ "output_index": tool.output_index, "item": {
            "id": tool.item_id, "type": "function_call", "status": "in_progress", "call_id": tool.call_id,
            "name": tool.name, "arguments": "" } });
        self.emit(out, "response.output_item.added", data);
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        self.close_reasoning(out);
        self.close_message(out);
        let pending: Vec<i64> = self.tools.keys().copied().collect();
        for index in pending {
            self.announce(out, index);
        }
        for tool in std::mem::take(&mut self.tools).into_values() {
            let item = json!({ "id": tool.item_id, "type": "function_call", "status": "completed",
                               "call_id": tool.call_id, "name": tool.name, "arguments": tool.arguments });
            self.emit(
                out,
                "response.function_call_arguments.done",
                json!({ "item_id": tool.item_id, "output_index": tool.output_index, "arguments": tool.arguments }),
            );
            self.emit(
                out,
                "response.output_item.done",
                json!({ "output_index": tool.output_index, "item": item.clone() }),
            );
            self.done_items.push((tool.output_index, item));
        }
        self.done_items.sort_by_key(|(index, _)| *index);
        let output: Vec<Value> = self.done_items.drain(..).map(|(_, item)| item).collect();
        let incomplete = match self.finish_reason.as_deref() {
            Some("length") => Some("max_output_tokens"),
            Some("content_filter") => Some("content_filter"),
            _ => None,
        };
        let mut response = self.response(if incomplete.is_some() {
            "incomplete"
        } else {
            "completed"
        });
        response["output"] = Value::Array(output);
        if let Some(reason) = incomplete {
            response["incomplete_details"] = json!({ "reason": reason });
        }
        if let Some(tokens) = &self.usage {
            response["usage"] = tokens.to_responses_usage();
        }
        if let Some(tier) = &self.service_tier {
            response["service_tier"] = json!(tier);
        }
        let kind = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.emit(out, kind, json!({ "response": response }));
    }
}

impl Decoder for ChatDecoder {
    fn push(&mut self, _event: &str, data: &Value, out: &mut Vec<Event>) -> Result<(), Failure> {
        if data == "[DONE]" {
            self.finish(out);
            return Ok(());
        }
        if data["error"].is_object() {
            let message = data["error"]["message"].as_str().unwrap_or("The upstream failed.");
            return Err(Failure::new(StatusCode::BAD_GATEWAY, "upstream_error", message));
        }
        if !self.started {
            self.started = true;
            if let Some(model) = data["model"].as_str() {
                self.model = model.to_owned();
            }
            let response = self.response("in_progress");
            self.emit(out, "response.created", json!({ "response": response }));
        }
        if data["usage"].is_object() {
            self.usage = Some(tokens(&data["usage"]));
        }
        if let Some(tier) = data["service_tier"].as_str() {
            self.service_tier = Some(tier.to_owned());
        }
        for choice in data["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            // Encrypted details can come without any reasoning text; they still need an item.
            if let Some(details) = delta["reasoning_details"].as_array().filter(|d| !d.is_empty()) {
                self.merge_details(&Value::Array(details.clone()));
                self.open_reasoning(out);
            }
            let reasoning = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str());
            if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
                self.reasoning_delta(out, text);
            }
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                let (item_id, index, part) = self.open_part(out, false);
                if let Some(message) = self.message.as_mut() {
                    message.text.push_str(text);
                }
                self.emit(
                    out,
                    "response.output_text.delta",
                    json!({ "item_id": item_id, "output_index": index,
                    "content_index": part, "delta": text }),
                );
            }
            if let Some(text) = delta["refusal"].as_str().filter(|t| !t.is_empty()) {
                let (item_id, index, part) = self.open_part(out, true);
                if let Some(message) = self.message.as_mut() {
                    message.refusal.push_str(text);
                }
                self.emit(
                    out,
                    "response.refusal.delta",
                    json!({ "item_id": item_id, "output_index": index,
                    "content_index": part, "delta": text }),
                );
            }
            for annotation in delta["annotations"].as_array().into_iter().flatten() {
                let citation = &annotation["url_citation"];
                if annotation["type"] == "url_citation" {
                    let (item_id, index, part) = self.open_part(out, false);
                    let converted = json!({ "type": "url_citation", "url": citation["url"], "title": citation["title"],
                                            "start_index": citation["start_index"], "end_index": citation["end_index"] });
                    self.annotations.push(converted.clone());
                    self.emit(
                        out,
                        "response.output_text.annotation.added",
                        json!({ "item_id": item_id, "output_index": index,
                        "content_index": part, "annotation": converted }),
                    );
                }
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"].as_i64().unwrap_or(0);
                if !self.tools.contains_key(&index) {
                    self.close_reasoning(out);
                    self.close_message(out);
                    let output_index = self.next_index;
                    self.next_index += 1;
                    let call_id = call["id"]
                        .as_str()
                        .map_or_else(|| format!("call_{}", random_token(12)), str::to_owned);
                    self.tools.insert(
                        index,
                        ToolCall {
                            item_id: format!("fc_{}", random_token(12)),
                            output_index,
                            call_id,
                            name: String::new(),
                            arguments: String::new(),
                            announced: false,
                        },
                    );
                }
                let tool = self.tools.get_mut(&index).expect("inserted");
                // Some upstreams split the name; it is complete when the arguments start.
                if let Some(name) = call["function"]["name"].as_str().filter(|_| !tool.announced) {
                    tool.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str().filter(|a| !a.is_empty()) {
                    self.announce(out, index);
                    let tool = self.tools.get_mut(&index).expect("inserted");
                    tool.arguments.push_str(arguments);
                    let (item_id, output_index) = (tool.item_id.clone(), tool.output_index);
                    self.emit(
                        out,
                        "response.function_call_arguments.delta",
                        json!({ "item_id": item_id, "output_index": output_index, "delta": arguments }),
                    );
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        Ok(())
    }

    fn tokens(&self) -> Option<Tokens> {
        self.usage.clone()
    }
}
