//! Anthropic Messages upstreams: the Responses request as a Messages request, and the Messages
//! stream as Responses events.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Map, Value, json};

use super::engine::{Event, Failure};
use super::provider::Decoder;
use super::routing::{Alias, Route};
use crate::crypto::random_token;
use crate::db::now;
use crate::usage::{Extras, Step, Tokens};

/// Anthropic thinking blocks go to Responses clients inside encrypted reasoning with this prefix.
const THINKING_PREFIX: &str = "aipa1:";
const FAST_BETA: &str = "fast-mode-2026-02-01";
const DEFAULT_MAX_TOKENS: i64 = 32_000;
const MIN_BUDGET: i64 = 1_024;
/// Upper bound for the kept assistant content put back into one request.
const MAX_RESTORED_BYTES: usize = 16 * 1024 * 1024;
const EFFORT_ORDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn bad(message: impl Into<String>) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message)
}

/// The Anthropic effort for an OpenAI effort, lowered or raised to what the model supports.
fn anthropic_effort(effort: &str, capabilities: &Value) -> &'static str {
    let wanted = match effort {
        "none" | "minimal" | "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        _ => "max",
    };
    let supported: Vec<&str> = capabilities["efforts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let rank = |e: &str| EFFORT_ORDER.iter().position(|x| *x == e).unwrap_or(0);
    if supported.is_empty() || supported.contains(&wanted) {
        return EFFORT_ORDER[rank(wanted)];
    }
    let nearest = EFFORT_ORDER
        .iter()
        .filter(|e| supported.contains(e))
        .min_by_key(|e| (rank(e).abs_diff(rank(wanted)), std::cmp::Reverse(rank(e))));
    nearest.copied().unwrap_or(EFFORT_ORDER[rank(wanted)])
}

fn budget(effort: &str) -> i64 {
    match effort {
        "none" | "minimal" | "low" => 2_000,
        "medium" => 8_000,
        "high" => 16_000,
        _ => 32_000,
    }
}

/// Sets `output_config.effort`, `thinking` and `max_tokens` for an effort. `client_max` is the
/// output limit that the client set.
fn apply_effort(body: &mut Value, effort: &str, client_max: Option<i64>, capabilities: &Value) {
    let model_max = capabilities["max_output"].as_i64();
    if effort == "none" {
        // Some models always think; they reject `disabled`, so they get the lowest effort.
        if capabilities["thinking_always_on"] != true {
            body["thinking"] = json!({ "type": "disabled" });
        } else if capabilities["efforts"].as_array().is_some_and(|e| !e.is_empty()) {
            body["output_config"]["effort"] = json!(anthropic_effort(effort, capabilities));
        }
        return;
    }
    if capabilities["efforts"].as_array().is_some_and(|e| !e.is_empty()) {
        body["output_config"]["effort"] = json!(anthropic_effort(effort, capabilities));
    }
    let thinking = &capabilities["thinking"];
    if thinking["adaptive"] == true {
        body["thinking"] = json!({ "type": "adaptive" });
        return;
    }
    if thinking["enabled"] == false {
        return;
    }
    let mut budget = budget(effort);
    match client_max {
        Some(max) => {
            budget = budget.min(max - 1);
            if budget < MIN_BUDGET {
                return;
            }
        }
        None => {
            let max = (budget + 8_000).min(model_max.unwrap_or(i64::MAX));
            body["max_tokens"] = json!(max);
            budget = budget.min(max - 1);
            if budget < MIN_BUDGET {
                return;
            }
        }
    }
    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
}

/// A thinking budget must stay below `max_tokens`. Too small a budget leaves thinking out.
pub fn fit_thinking_budget(body: &mut Value) {
    let (Some(budget), Some(max)) = (body["thinking"]["budget_tokens"].as_i64(), body["max_tokens"].as_i64()) else {
        return;
    };
    if budget < max {
        return;
    }
    if max > MIN_BUDGET {
        body["thinking"]["budget_tokens"] = json!(max - 1);
    } else if let Some(map) = body.as_object_mut() {
        map.remove("thinking");
    }
}

/// Anthropic refuses forced tools with manual thinking, and some models also with adaptive
/// thinking. Then thinking is left out for the request.
/// Turns thinking off. A missing field is not enough: newer models think by default. Opus 5
/// refuses `disabled` above effort `high`.
fn turn_thinking_off(body: &mut Value) {
    body["thinking"] = json!({ "type": "disabled" });
    if matches!(body["output_config"]["effort"].as_str(), Some("xhigh" | "max")) {
        body["output_config"]["effort"] = json!("high");
    }
}

/// Forced tools (`any` or a named tool) do not work with manual thinking, so it is turned off
/// for such a request. Models with `forced_tools_with_thinking: false` refuse forced tools on
/// every request; they get a 400.
fn drop_thinking_for_forced_tools(body: &mut Value, capabilities: &Value) -> Result<(), &'static str> {
    if !matches!(body["tool_choice"]["type"].as_str(), Some("any" | "tool")) {
        return Ok(());
    }
    if capabilities["forced_tools_with_thinking"] == false {
        return Err("This model does not support a forced tool choice. Use tool choice auto.");
    }
    if body["thinking"]["type"] == "enabled" {
        turn_thinking_off(body);
    }
    Ok(())
}

/// Anthropic refuses sampling options with thinking, except the defaults.
fn drop_sampling_with_thinking(body: &mut Value) {
    if matches!(body["thinking"]["type"].as_str(), Some("enabled" | "adaptive"))
        && let Some(map) = body.as_object_mut()
    {
        for field in ["temperature", "top_p", "top_k"] {
            map.remove(field);
        }
    }
}

/// Web search tools call search directly (newer versions otherwise start code execution,
/// which the gateway does not allow).
fn direct_search(tool: &mut Value) -> Result<(), Failure> {
    match tool["allowed_callers"].as_array() {
        None => tool["allowed_callers"] = json!(["direct"]),
        Some(callers) if callers.iter().all(|c| c == "direct") => {}
        Some(_) => return Err(bad("Web search can only use `allowed_callers: [\"direct\"]`.")),
    }
    Ok(())
}

/// Content blocks that native requests may carry. Other blocks (for example inline tool
/// definitions of newer betas, containers or MCP results) could add hosted tools or other
/// models without the gateway's checks.
const NATIVE_BLOCKS: [&str; 10] = [
    "text",
    "image",
    "document",
    "search_result",
    "tool_use",
    "tool_result",
    "thinking",
    "redacted_thinking",
    "server_tool_use",
    "web_search_tool_result",
];

fn check_native_blocks(body: &Value) -> Result<(), Failure> {
    for message in body["messages"].as_array().into_iter().flatten() {
        for block in message["content"].as_array().into_iter().flatten() {
            let kind = block["type"].as_str().unwrap_or_default();
            let known = NATIVE_BLOCKS.contains(&kind) && (kind != "server_tool_use" || block["name"] == "web_search");
            if !known {
                return Err(bad(format!("The content block `{kind}` is not supported.")));
            }
            check_source(block)?;
            let nested = block["content"].as_array().into_iter().flatten();
            for inner in nested.filter(|_| kind == "tool_result") {
                if !matches!(
                    inner["type"].as_str(),
                    Some("text" | "image" | "document" | "search_result")
                ) {
                    return Err(bad(
                        "A tool result can hold only text, image, document and search result blocks.",
                    ));
                }
                check_source(inner)?;
            }
        }
    }
    Ok(())
}

/// Images and documents may carry only inline data or URLs, also inside inline documents: a
/// stored file would be read with the shared provider key.
fn check_source(block: &Value) -> Result<(), Failure> {
    let source = &block["source"];
    if source.is_null() {
        return Ok(());
    }
    if !matches!(source["type"].as_str(), Some("base64" | "url" | "text" | "content")) || !source["file_id"].is_null() {
        return Err(bad(
            "References to stored files are not supported. Send the content inline.",
        ));
    }
    for nested in source["content"].as_array().into_iter().flatten() {
        check_source(nested)?;
    }
    Ok(())
}

/// Adjusts a native Messages body: web search callers, alias defaults and fast mode. Returns
/// the beta headers to add.
pub fn prepare_native(body: &mut Value, alias: Option<&Alias>, capabilities: &Value) -> Result<Vec<String>, Failure> {
    if !body["fallbacks"].is_null() {
        return Err(bad(
            "`fallbacks` is not supported: a fallback model would skip the key's model list.",
        ));
    }
    // Hosted connectors and containers reuse provider-side objects under the shared key.
    for field in ["mcp_servers", "container"] {
        if !body[field].is_null() {
            return Err(bad(format!("`{field}` is not supported.")));
        }
    }
    // `get_mut`: indexing a missing field would add it as null, which Anthropic refuses.
    for tool in body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if tool["type"].as_str().is_some_and(|k| k.starts_with("web_search")) {
            direct_search(tool)?;
        }
    }
    check_native_blocks(body)?;
    // Thinking that the gateway made from OpenAI reasoning has no Anthropic signature.
    for message in body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
            content.retain(|block| {
                !(block["type"] == "thinking"
                    && block["signature"]
                        .as_str()
                        .is_some_and(|s| s.starts_with(super::messages_api::SIGNATURE_PREFIX)))
            });
        }
    }
    // A limit above the model maximum is lowered to it (Anthropic refuses it).
    if let (Some(max), Some(model_max)) = (body["max_tokens"].as_i64(), capabilities["max_output"].as_i64())
        && max > model_max
    {
        body["max_tokens"] = json!(model_max);
        fit_thinking_budget(body);
    }
    if let Some(alias) = alias {
        if let Some(effort) = &alias.effort
            && body["output_config"]["effort"].is_null()
        {
            match body["thinking"]["type"].as_str() {
                None => {
                    let client_max = body["max_tokens"].as_i64();
                    apply_effort(body, effort, client_max, capabilities);
                    drop_sampling_with_thinking(body);
                }
                // Adaptive thinking picks the mode, not the effort; the effort default still applies.
                Some("adaptive") if capabilities["efforts"].as_array().is_some_and(|e| !e.is_empty()) => {
                    body["output_config"]["effort"] = json!(anthropic_effort(effort, capabilities));
                }
                _ => {}
            }
        }
        if alias.fast && body["speed"].is_null() && capabilities["fast"] == true {
            body["speed"] = json!("fast");
        }
    }
    // An explicit effort also changes to what the model supports. A model without efforts
    // gets the thinking budget of that effort instead.
    if let Some(effort) = body["output_config"]["effort"].as_str().map(str::to_owned) {
        if capabilities["efforts"].as_array().is_some_and(Vec::is_empty) {
            if let Some(config) = body["output_config"].as_object_mut() {
                config.remove("effort");
            }
            if body["thinking"].is_null() {
                let client_max = body["max_tokens"].as_i64();
                apply_effort(body, &effort, client_max, capabilities);
                drop_sampling_with_thinking(body);
            }
        } else {
            body["output_config"]["effort"] = json!(anthropic_effort(&effort, capabilities));
        }
    }
    drop_thinking_for_forced_tools(body, capabilities).map_err(bad)?;
    // A model without fast mode rejects `speed`; it answers at normal speed instead.
    if capabilities["fast"] == false
        && let Some(map) = body.as_object_mut()
    {
        map.remove("speed");
    }
    let mut betas = Vec::new();
    if body["speed"] == "fast" {
        betas.push(FAST_BETA.to_owned());
    }
    Ok(betas)
}

// ---------------------------------------------------------------------------------------------
// Request

/// A Responses content part as an Anthropic block.
fn block(part: &Value) -> Result<Option<Value>, String> {
    let data_url = |url: &str| -> Option<(String, String)> {
        let rest = url.strip_prefix("data:")?;
        let (media_type, data) = rest.split_once(";base64,")?;
        Some((media_type.to_owned(), data.to_owned()))
    };
    Ok(match part["type"].as_str().unwrap_or_default() {
        "input_text" | "output_text" | "text" => Some(json!({ "type": "text", "text": part["text"] })),
        "refusal" => Some(json!({ "type": "text", "text": part["refusal"] })),
        "input_image" => {
            let url = part["image_url"].as_str().unwrap_or_default();
            Some(match data_url(url) {
                Some((media_type, data)) => {
                    json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } })
                }
                None => json!({ "type": "image", "source": { "type": "url", "url": url } }),
            })
        }
        "input_file" => {
            if let Some((media_type, data)) = part["file_data"].as_str().and_then(data_url) {
                Some(
                    json!({ "type": "document", "source": { "type": "base64", "media_type": media_type, "data": data } }),
                )
            } else if let Some(url) = part["file_url"].as_str() {
                Some(json!({ "type": "document", "source": { "type": "url", "url": url } }))
            } else {
                return Err("Anthropic models take files as base64 data or a URL.".into());
            }
        }
        "input_audio" => return Err("Anthropic models do not accept audio input.".into()),
        other => return Err(format!("The content type `{other}` is not supported for this model.")),
    })
}

fn blocks(content: &Value) -> Result<Vec<Value>, String> {
    match content {
        Value::String(text) => Ok(vec![json!({ "type": "text", "text": text })]),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                out.extend(block(part)?);
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

/// A thinking or redacted thinking block with only its own fields, else nothing.
fn thinking_block_only(block: &Value) -> Option<Value> {
    match block["type"].as_str()? {
        "thinking" => Some(
            json!({ "type": "thinking", "thinking": block["thinking"].as_str()?, "signature": block["signature"].as_str()? }),
        ),
        "redacted_thinking" => Some(json!({ "type": "redacted_thinking", "data": block["data"].as_str()? })),
        _ => None,
    }
}

/// Adds blocks to the conversation. Anthropic needs alternating roles, so blocks of the same
/// role join the last message.
fn push(messages: &mut Vec<Value>, role: &str, mut content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == role)
        && let Some(existing) = last["content"].as_array_mut()
    {
        existing.append(&mut content);
        return;
    }
    messages.push(json!({ "role": role, "content": content }));
}

/// Converts a Responses body to a Messages body. Returns the body and the beta headers.
/// `restore` gives the kept assistant content of a tool call (see `thinking_cache`).
pub fn encode(
    body: &Value,
    route: &Route,
    restore: &dyn Fn(&str) -> Option<Arc<Vec<Value>>>,
) -> Result<(Value, Vec<String>), String> {
    let capabilities = &route.capabilities;
    let mut system: Vec<String> = Vec::new();
    if let Some(instructions) = body["instructions"].as_str().filter(|i| !i.is_empty()) {
        system.push(instructions.to_owned());
    }
    let items = match &body["input"] {
        Value::String(text) => vec![json!({ "type": "message", "role": "user", "content": text })],
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    };
    let mut messages: Vec<Value> = Vec::new();
    for item in &items {
        match item["type"].as_str().unwrap_or("message") {
            "message" => match item["role"].as_str().unwrap_or("user") {
                "system" | "developer" => {
                    let text: Vec<String> = blocks(&item["content"])?
                        .iter()
                        .filter_map(|b| b["text"].as_str().map(str::to_owned))
                        .collect();
                    system.push(text.join("\n"));
                }
                "assistant" => push(&mut messages, "assistant", blocks(&item["content"])?),
                _ => push(&mut messages, "user", blocks(&item["content"])?),
            },
            "reasoning" => {
                // Only thinking that came from Anthropic can go back. The client can change the
                // envelope, so only thinking blocks with their known fields pass.
                let thinking = item["encrypted_content"]
                    .as_str()
                    .and_then(|e| e.strip_prefix(THINKING_PREFIX))
                    .and_then(|e| b64().decode(e).ok())
                    .and_then(|bytes| serde_json::from_slice::<Vec<Value>>(&bytes).ok());
                if let Some(thinking) = thinking {
                    push(
                        &mut messages,
                        "assistant",
                        thinking.iter().filter_map(thinking_block_only).collect(),
                    );
                }
            }
            "function_call" => {
                let input: Value =
                    serde_json::from_str(item["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                push(
                    &mut messages,
                    "assistant",
                    vec![json!({ "type": "tool_use", "id": item["call_id"], "name": item["name"], "input": input })],
                );
            }
            "function_call_output" => {
                let content = match &item["output"] {
                    Value::String(text) => json!(text),
                    other => Value::Array(blocks(other)?),
                };
                let mut result = json!({ "type": "tool_result", "tool_use_id": item["call_id"], "content": content });
                if item["output"].as_str().is_some_and(|o| o.starts_with("Error:")) {
                    result["is_error"] = json!(true);
                }
                push(&mut messages, "user", vec![result]);
            }
            // Earlier searches cannot go back without their encrypted results; the answer text
            // stays in the history.
            "web_search_call" => {}
            other => return Err(format!("The input item `{other}` is not supported for this model.")),
        }
    }

    // Every assistant turn gets its kept content back: newer models check that the history
    // before a signed block is unchanged. Each kept entry is used once, and the total is
    // bounded, so a client cannot grow the request by repeating tool call ids.
    let mut used: Vec<*const Vec<Value>> = Vec::new();
    let mut restored_bytes = 0usize;
    for message in messages.iter_mut().filter(|m| m["role"] == "assistant") {
        let kept = message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .find_map(|b| b["id"].as_str().and_then(restore));
        let Some(content) = kept else { continue };
        if used.contains(&Arc::as_ptr(&content)) {
            continue;
        }
        restored_bytes += serde_json::to_vec(content.as_ref()).map_or(0, |bytes| bytes.len());
        if restored_bytes > MAX_RESTORED_BYTES {
            return Err("The conversation is too long to restore its thinking. Start the tool loop again.".into());
        }
        used.push(Arc::as_ptr(&content));
        message["content"] = Value::Array(content.as_ref().clone());
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(route.upstream_model));
    if !system.is_empty() {
        out.insert("system".into(), json!(system.join("\n\n")));
    }
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(true));
    let model_max = capabilities["max_output"].as_i64();
    let client_max = body["max_output_tokens"].as_i64();
    let max_tokens = client_max.unwrap_or(model_max.unwrap_or(DEFAULT_MAX_TOKENS));
    let max_tokens = model_max.map_or(max_tokens, |m| max_tokens.min(m));
    out.insert("max_tokens".into(), json!(max_tokens));

    let mut tools = Vec::new();
    for tool in body["tools"].as_array().into_iter().flatten() {
        match tool["type"].as_str().unwrap_or_default() {
            "function" => {
                let schema = if tool["parameters"].is_object() {
                    tool["parameters"].clone()
                } else {
                    json!({ "type": "object" })
                };
                let mut function = json!({ "name": tool["name"], "input_schema": schema });
                if tool["strict"] == true {
                    function["strict"] = json!(true);
                }
                if let Some(description) = tool["description"].as_str() {
                    function["description"] = json!(description);
                }
                tools.push(function);
            }
            "web_search" => {
                let mut search =
                    json!({ "type": "web_search_20250305", "name": "web_search", "allowed_callers": ["direct"] });
                if let Some(domains) = tool["filters"]["allowed_domains"].as_array() {
                    search["allowed_domains"] = json!(domains);
                }
                if tool["user_location"].is_object() {
                    search["user_location"] = tool["user_location"].clone();
                }
                tools.push(search);
            }
            other => return Err(format!("The tool type `{other}` is not supported for this model.")),
        }
    }
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        let mut choice = match &body["tool_choice"] {
            Value::String(choice) if choice == "required" => json!({ "type": "any" }),
            Value::String(choice) if choice == "none" => json!({ "type": "none" }),
            Value::Object(choice) if choice.get("type").and_then(Value::as_str) == Some("function") => {
                json!({ "type": "tool", "name": choice["name"] })
            }
            Value::Object(choice) if choice.get("type").and_then(Value::as_str) == Some("web_search") => {
                json!({ "type": "tool", "name": "web_search" })
            }
            _ => json!({ "type": "auto" }),
        };
        if body["parallel_tool_calls"] == false && choice["type"] != "none" {
            choice["disable_parallel_tool_use"] = json!(true);
        }
        out.insert("tool_choice".into(), choice);
    }

    let mut body_out = Value::Object(out);
    if let Some(effort) = body["reasoning"]["effort"].as_str() {
        // A client limit counts as lowered to the model maximum.
        apply_effort(&mut body_out, effort, client_max.map(|_| max_tokens), capabilities);
    }
    // Anthropic needs the thinking blocks of the last assistant turn with tool calls. Without
    // them (for example a client that cannot keep them), thinking is left out.
    let last_assistant = body_out["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "assistant"));
    let has_tool_use = last_assistant.is_some_and(|m| {
        m["content"]
            .as_array()
            .is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use"))
    });
    let has_thinking = last_assistant.is_some_and(|m| {
        m["content"].as_array().is_some_and(|c| {
            c.iter()
                .any(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
        })
    });
    if has_tool_use && !has_thinking {
        if capabilities["thinking_always_on"] == true {
            return Err(
                "The thinking of the last tool call is not known any more (for example after a restart). Start the tool loop again."
                    .into(),
            );
        }
        turn_thinking_off(&mut body_out);
    }
    drop_thinking_for_forced_tools(&mut body_out, capabilities)?;
    let thinking_on = matches!(body_out["thinking"]["type"].as_str(), Some("enabled" | "adaptive"));
    if !thinking_on {
        for field in ["temperature", "top_p"] {
            if !body[field].is_null() {
                body_out[field] = body[field].clone();
            }
        }
    }
    if let Some(stops) = body["stop"].as_array() {
        body_out["stop_sequences"] = json!(stops);
    }
    let format = &body["text"]["format"];
    match format["type"].as_str() {
        Some("json_schema") => {
            body_out["output_config"]["format"] = json!({ "type": "json_schema", "schema": format["schema"] });
        }
        Some("json_object") => {
            return Err("Anthropic models have no JSON object mode. Use a JSON schema.".into());
        }
        _ => {}
    }
    let mut betas = Vec::new();
    if matches!(body["service_tier"].as_str(), Some("priority" | "fast")) && capabilities["fast"] == true {
        body_out["speed"] = json!("fast");
        betas.push(FAST_BETA.to_owned());
    }
    Ok((body_out, betas))
}

// ---------------------------------------------------------------------------------------------
// Stream

enum Block {
    Text {
        item_id: String,
        index: usize,
        text: String,
        annotations: Vec<Value>,
    },
    Thinking {
        item_id: String,
        index: usize,
        block: Value,
    },
    Tool {
        item_id: String,
        index: usize,
        call_id: String,
        name: String,
        arguments: String,
    },
    Search {
        item_id: String,
        index: usize,
        query: String,
    },
    Other,
}

/// Messages events as Responses events. One Anthropic block becomes one output item.
pub struct MessagesDecoder {
    ttl: Option<String>,
    id: String,
    model: String,
    created: i64,
    started: bool,
    sequence: u64,
    next_index: usize,
    blocks: HashMap<i64, Block>,
    /// Search items that wait for their results, by tool use id.
    searches: HashMap<String, (String, usize, String)>,
    /// The complete blocks of the answer, for a continuation after `pause_turn`.
    content: Vec<Value>,
    raw: HashMap<i64, Value>,
    usage: Value,
    /// The sampling steps of the answers that ended (answers, and iterations in them).
    steps: Vec<Step>,
    /// The facts of the last answer that ended.
    extras: Extras,
    stop_reason: Option<String>,
    output: Vec<(usize, Value)>,
    service_tier: Option<String>,
    /// `message_stop` arrived for the current upstream answer.
    stopped: bool,
    /// Upstream answers that ended with their usage.
    answers: usize,
    /// Characters streamed in the current upstream answer.
    answer_chars: usize,
}

impl MessagesDecoder {
    pub fn new(ttl: Option<String>) -> Self {
        Self {
            ttl,
            id: format!("resp_{}", random_token(12)),
            model: String::new(),
            created: now(),
            started: false,
            sequence: 0,
            next_index: 0,
            blocks: HashMap::new(),
            searches: HashMap::new(),
            content: Vec::new(),
            raw: HashMap::new(),
            usage: Value::Null,
            steps: Vec::new(),
            extras: Extras::default(),
            stop_reason: None,
            output: Vec::new(),
            service_tier: None,
            stopped: false,
            answers: 0,
            answer_chars: 0,
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

    fn next_index(&mut self) -> usize {
        self.next_index += 1;
        self.next_index - 1
    }

    fn done(&mut self, out: &mut Vec<Event>, index: usize, item: Value) {
        self.emit(
            out,
            "response.output_item.done",
            json!({ "output_index": index, "item": item.clone() }),
        );
        self.output.push((index, item));
    }

    fn start_block(&mut self, out: &mut Vec<Event>, position: i64, block: &Value) {
        self.raw.insert(position, block.clone());
        let entry = match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let (item_id, index) = (format!("msg_{}", random_token(12)), self.next_index());
                self.emit(
                    out,
                    "response.output_item.added",
                    json!({ "output_index": index, "item": {
                    "id": item_id, "type": "message", "role": "assistant", "status": "in_progress", "content": [] } }),
                );
                self.emit(
                    out,
                    "response.content_part.added",
                    json!({ "item_id": item_id, "output_index": index,
                    "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } }),
                );
                let text = block["text"].as_str().unwrap_or_default().to_owned();
                if !text.is_empty() {
                    self.emit(
                        out,
                        "response.output_text.delta",
                        json!({ "item_id": item_id, "output_index": index, "content_index": 0, "delta": text }),
                    );
                }
                Block::Text {
                    item_id,
                    index,
                    text,
                    annotations: Vec::new(),
                }
            }
            "thinking" | "redacted_thinking" => {
                let (item_id, index) = (format!("rs_{}", random_token(12)), self.next_index());
                self.emit(
                    out,
                    "response.output_item.added",
                    json!({ "output_index": index, "item": {
                    "id": item_id, "type": "reasoning", "summary": [] } }),
                );
                if block["type"] == "thinking" {
                    self.emit(
                        out,
                        "response.reasoning_summary_part.added",
                        json!({ "item_id": item_id, "output_index": index,
                        "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
                    );
                }
                Block::Thinking {
                    item_id,
                    index,
                    block: block.clone(),
                }
            }
            "tool_use" => {
                let (item_id, index) = (format!("fc_{}", random_token(12)), self.next_index());
                let call_id = block["id"].as_str().unwrap_or_default().to_owned();
                let name = block["name"].as_str().unwrap_or_default().to_owned();
                self.emit(out, "response.output_item.added", json!({ "output_index": index, "item": {
                    "id": item_id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": "" } }));
                Block::Tool {
                    item_id,
                    index,
                    call_id,
                    name,
                    arguments: String::new(),
                }
            }
            "server_tool_use" if block["name"] == "web_search" => {
                let (item_id, index) = (format!("ws_{}", random_token(12)), self.next_index());
                self.emit(
                    out,
                    "response.output_item.added",
                    json!({ "output_index": index, "item": {
                    "id": item_id, "type": "web_search_call", "status": "in_progress" } }),
                );
                Block::Search {
                    item_id,
                    index,
                    query: String::new(),
                }
            }
            "web_search_tool_result" => {
                let tool_use_id = block["tool_use_id"].as_str().unwrap_or_default();
                if let Some((item_id, index, query)) = self.searches.remove(tool_use_id) {
                    // An error (for example `max_uses_exceeded`) is an object instead of a list;
                    // such a search is failed and not billed.
                    let status = if block["content"].is_array() {
                        "completed"
                    } else {
                        "failed"
                    };
                    let sources: Vec<Value> = block["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|r| json!({ "type": "url", "url": r["url"], "title": r["title"] }))
                        .collect();
                    let item = json!({ "id": item_id, "type": "web_search_call", "status": status,
                                       "action": { "type": "search", "query": query, "sources": sources } });
                    self.done(out, index, item);
                }
                Block::Other
            }
            _ => Block::Other,
        };
        self.blocks.insert(position, entry);
    }

    fn delta(&mut self, out: &mut Vec<Event>, position: i64, delta: &Value) {
        if let Some(raw) = self.raw.get_mut(&position) {
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => append(raw, "text", &delta["text"]),
                "thinking_delta" => append(raw, "thinking", &delta["thinking"]),
                "signature_delta" => append(raw, "signature", &delta["signature"]),
                "input_json_delta" => append(raw, "partial_json", &delta["partial_json"]),
                "citations_delta" => {
                    let mut list = raw["citations"].as_array().cloned().unwrap_or_default();
                    list.push(delta["citation"].clone());
                    raw["citations"] = Value::Array(list);
                }
                _ => {}
            }
        }
        let mut events: Vec<(&'static str, Value)> = Vec::new();
        match self.blocks.get_mut(&position) {
            Some(Block::Text {
                item_id,
                index,
                text,
                annotations,
            }) => match delta["type"].as_str() {
                Some("text_delta") => {
                    let piece = delta["text"].as_str().unwrap_or_default();
                    text.push_str(piece);
                    events.push((
                        "response.output_text.delta",
                        json!({ "item_id": item_id, "output_index": index, "content_index": 0, "delta": piece }),
                    ));
                }
                Some("citations_delta") => {
                    let citation = &delta["citation"];
                    let length = text.chars().count();
                    let annotation = json!({ "type": "url_citation", "url": citation["url"], "title": citation["title"],
                                             "start_index": length, "end_index": length });
                    annotations.push(annotation.clone());
                    events.push((
                        "response.output_text.annotation.added",
                        json!({ "item_id": item_id, "output_index": index,
                        "content_index": 0, "annotation": annotation }),
                    ));
                }
                _ => {}
            },
            Some(Block::Thinking { item_id, index, block }) => match delta["type"].as_str() {
                Some("thinking_delta") => {
                    let piece = delta["thinking"].as_str().unwrap_or_default();
                    append(block, "thinking", &json!(piece));
                    events.push((
                        "response.reasoning_summary_text.delta",
                        json!({ "item_id": item_id, "output_index": index, "summary_index": 0, "delta": piece }),
                    ));
                }
                Some("signature_delta") => append(block, "signature", &delta["signature"]),
                _ => {}
            },
            Some(Block::Tool {
                item_id,
                index,
                arguments,
                ..
            }) => {
                if let Some(piece) = delta["partial_json"].as_str() {
                    arguments.push_str(piece);
                    events.push((
                        "response.function_call_arguments.delta",
                        json!({ "item_id": item_id, "output_index": index, "delta": piece }),
                    ));
                }
            }
            Some(Block::Search { query, .. }) => {
                if let Some(piece) = delta["partial_json"].as_str() {
                    query.push_str(piece);
                }
            }
            _ => {}
        }
        for (kind, data) in events {
            self.emit(out, kind, data);
        }
    }

    fn stop_block(&mut self, out: &mut Vec<Event>, position: i64) {
        if let Some(mut raw) = self.raw.remove(&position) {
            if let Some(partial) = raw.get("partial_json").and_then(Value::as_str) {
                let input: Value = serde_json::from_str(partial).unwrap_or(json!({}));
                raw["input"] = input;
                if let Some(map) = raw.as_object_mut() {
                    map.remove("partial_json");
                }
            }
            self.content.push(raw);
        }
        match self.blocks.remove(&position) {
            Some(Block::Text {
                item_id,
                index,
                text,
                annotations,
            }) => {
                let part = json!({ "type": "output_text", "text": text, "annotations": annotations });
                self.emit(
                    out,
                    "response.output_text.done",
                    json!({ "item_id": item_id, "output_index": index, "content_index": 0, "text": text }),
                );
                self.emit(
                    out,
                    "response.content_part.done",
                    json!({ "item_id": item_id, "output_index": index, "content_index": 0, "part": part.clone() }),
                );
                let item = json!({ "id": item_id, "type": "message", "role": "assistant", "status": "completed", "content": [part] });
                self.done(out, index, item);
            }
            Some(Block::Thinking { item_id, index, block }) => {
                let text = block["thinking"].as_str().unwrap_or_default().to_owned();
                let summary = if text.is_empty() {
                    json!([])
                } else {
                    json!([{ "type": "summary_text", "text": text }])
                };
                let encoded = b64().encode(serde_json::to_vec(&vec![block]).unwrap_or_default());
                let item = json!({ "id": item_id, "type": "reasoning", "summary": summary,
                                   "encrypted_content": format!("{THINKING_PREFIX}{encoded}") });
                if !text.is_empty() {
                    self.emit(
                        out,
                        "response.reasoning_summary_text.done",
                        json!({ "item_id": item_id, "output_index": index, "summary_index": 0, "text": text }),
                    );
                }
                self.done(out, index, item);
            }
            Some(Block::Tool {
                item_id,
                index,
                call_id,
                name,
                arguments,
            }) => {
                // A call without arguments gets `{}`, also as a delta: stream clients build the
                // arguments from the deltas.
                let arguments = if arguments.trim().is_empty() {
                    self.emit(
                        out,
                        "response.function_call_arguments.delta",
                        json!({ "item_id": item_id, "output_index": index, "delta": "{}" }),
                    );
                    "{}".to_owned()
                } else {
                    arguments
                };
                self.emit(
                    out,
                    "response.function_call_arguments.done",
                    json!({ "item_id": item_id, "output_index": index, "arguments": arguments }),
                );
                let item = json!({ "id": item_id, "type": "function_call", "status": "completed",
                                   "call_id": call_id, "name": name, "arguments": arguments });
                self.done(out, index, item);
            }
            Some(Block::Search { item_id, index, query }) => {
                let query = serde_json::from_str::<Value>(&query)
                    .ok()
                    .and_then(|q| q["query"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                let tool_use_id = self
                    .content
                    .last()
                    .and_then(|b| b["id"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                self.searches.insert(tool_use_id, (item_id, index, query));
            }
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        // A search without results still ends as an item.
        for (_, (item_id, index, query)) in std::mem::take(&mut self.searches) {
            let item = json!({ "id": item_id, "type": "web_search_call", "status": "completed",
                               "action": { "type": "search", "query": query } });
            self.done(out, index, item);
        }
        self.output.sort_by_key(|(index, _)| *index);
        let output: Vec<Value> = self.output.drain(..).map(|(_, item)| item).collect();
        let incomplete = match self.stop_reason.as_deref() {
            Some("max_tokens" | "pause_turn" | "model_context_window_exceeded") => Some("max_output_tokens"),
            Some("refusal") => Some("content_filter"),
            _ => None,
        };
        let mut response = json!({ "id": self.id, "object": "response", "created_at": self.created, "model": self.model,
                                   "status": if incomplete.is_some() { "incomplete" } else { "completed" },
                                   "output": output, "usage": Step::total(&self.steps).to_responses_usage() });
        if let Some(tier) = &self.service_tier {
            response["service_tier"] = json!(tier);
        }
        if let Some(reason) = incomplete {
            response["incomplete_details"] = json!({ "reason": reason });
        }
        let kind = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.emit(out, kind, json!({ "response": response }));
    }

    /// Adds the usage of one upstream answer to the total.
    fn add_usage(&mut self) {
        self.answers += 1;
        self.answer_chars = 0;
        self.steps.extend(Step::anthropic(&self.usage, self.ttl.as_deref()));
        self.extras = Extras::from_usage(&self.usage);
        self.usage = Value::Null;
    }

    /// A paused answer that ended with `message_stop`, so its usage is known.
    fn paused(&self) -> bool {
        self.stopped && self.stop_reason.as_deref() == Some("pause_turn")
    }
}

/// Appends in place: a copy of the whole text per delta would make long streams quadratic.
fn append(target: &mut Value, field: &str, piece: &Value) {
    let piece = piece.as_str().unwrap_or_default();
    match &mut target[field] {
        Value::String(text) => text.push_str(piece),
        other => *other = json!(piece),
    }
}

impl Decoder for MessagesDecoder {
    fn push(&mut self, event: &str, data: &Value, out: &mut Vec<Event>) -> Result<(), Failure> {
        let position = data["index"].as_i64().unwrap_or(0);
        match data["type"].as_str().unwrap_or(event) {
            "message_start" => {
                self.usage = data["message"]["usage"].clone();
                if let Some(tier) = self.usage["service_tier"].as_str() {
                    // Anthropic says `standard` where OpenAI says `default`.
                    let tier = if tier == "standard" { "default" } else { tier };
                    self.service_tier = Some(tier.to_owned());
                }
                if !self.started {
                    self.started = true;
                    self.model = data["message"]["model"].as_str().unwrap_or_default().to_owned();
                    let response = json!({ "id": self.id, "object": "response", "created_at": self.created,
                                           "model": self.model, "status": "in_progress", "output": [] });
                    self.emit(out, "response.created", json!({ "response": response }));
                }
            }
            "content_block_start" => self.start_block(out, position, &data["content_block"]),
            "content_block_delta" => {
                let delta = &data["delta"];
                for field in ["text", "thinking", "partial_json"] {
                    self.answer_chars += delta[field].as_str().map_or(0, str::len);
                }
                self.delta(out, position, delta);
            }
            "content_block_stop" => self.stop_block(out, position),
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                for (key, value) in data["usage"].as_object().into_iter().flatten() {
                    if !value.is_null() {
                        self.usage[key] = value.clone();
                    }
                }
            }
            "message_stop" => {
                self.stopped = true;
                self.add_usage();
                if !self.paused() {
                    self.finish(out);
                }
            }
            "error" => {
                let message = data["error"]["message"].as_str().unwrap_or("The upstream failed.");
                let status = match data["error"]["type"].as_str() {
                    Some("overloaded_error") => StatusCode::SERVICE_UNAVAILABLE,
                    Some("rate_limit_error") => StatusCode::TOO_MANY_REQUESTS,
                    _ => StatusCode::BAD_GATEWAY,
                };
                return Err(Failure::new(status, "upstream_error", message));
            }
            _ => {}
        }
        Ok(())
    }

    /// Unknown until the first upstream answer ended; then the engine estimates instead.
    fn tokens(&self) -> Option<Tokens> {
        (self.answers > 0).then(|| Step::total(&self.steps()))
    }

    /// A continuation that broke off adds what its usage showed so far, as a step that is not
    /// exact.
    fn steps(&self) -> Vec<Step> {
        if self.answers == 0 {
            return Vec::new();
        }
        let mut steps = self.steps.clone();
        if self.usage.is_object() {
            let mut partial = Tokens::from_anthropic(&self.usage, self.ttl.as_deref());
            // The final usage of the broken answer is missing: its streamed text counts at
            // least (about 4 characters per token).
            partial.output_text = partial.output_text.max((self.answer_chars / 4) as i64);
            partial.inexact = true;
            steps.push(Step {
                tokens: partial,
                extras: Extras::from_usage(&self.usage),
                service_tier: self.usage["service_tier"].as_str().map(str::to_owned),
                estimated: true,
            });
        }
        steps
    }

    fn continuation(&mut self) -> Option<Vec<Value>> {
        if !self.paused() {
            return None;
        }
        self.stop_reason = None;
        self.stopped = false;
        Some(self.content.clone())
    }

    /// Only a paused answer that cannot continue ends here; a stream that closed before
    /// `message_stop` is a failure.
    fn end(&mut self, out: &mut Vec<Event>) {
        if self.paused() {
            self.finish(out);
        }
    }

    fn output_tokens(&self) -> i64 {
        Step::total(&self.steps).output()
    }

    fn extras(&self) -> Extras {
        self.extras.clone()
    }

    fn assistant_content(&self) -> &[Value] {
        &self.content
    }
}
