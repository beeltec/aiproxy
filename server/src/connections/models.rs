//! The model list of a connection, with capabilities.
//!
//! Capability keys (all optional; a missing key means "unknown"):
//! - `input`: input kinds (`text`, `image`, `file`, `audio`, `video`)
//! - `efforts`: reasoning efforts; `fast`: fast mode (priority tier or Anthropic `speed`)
//! - `context_window`, `max_output`: token limits
//! - `endpoints`: OpenAI endpoints (`chat`, `responses`); `chat_tools`: false when function tools
//!   work only on Responses; `mode`: the LiteLLM mode (`chat`, `embedding`, ...)
//! - `thinking`: Anthropic thinking types (`adaptive`, `enabled`); `forced_tools_with_thinking`
//!   (false: forced tools do not work with adaptive thinking; unknown means they do);
//!   `thinking_always_on`: the model cannot turn thinking off

use std::time::Duration;

use anyhow::bail;
use serde_json::{Map, Value, json};

use super::catalog::{self, Catalog};
use super::{Connection, Kind};
use crate::db::now;
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PAGES: usize = 20;
const MAX_LIST_BYTES: usize = 32 * 1024 * 1024;

struct Listed {
    id: String,
    display_name: Option<String>,
    capabilities: Value,
}

/// Loads the model list and stores it. New models start disabled. Returns the model count.
pub async fn sync(state: &AppState, connection_id: i64) -> anyhow::Result<usize> {
    let result = load(state, connection_id).await;
    let error = result.as_ref().err().map(|err| format!("{err:#}"));
    sqlx::query("UPDATE connections SET last_sync_at = ?, last_error = ? WHERE id = ?")
        .bind(now())
        .bind(&error)
        .bind(connection_id)
        .execute(&state.db)
        .await?;
    let listed = result?;

    let now = now();
    let source = connection_id.to_string();
    let mut tx = state.db.begin().await?;
    for model in &listed {
        sqlx::query(
            "INSERT INTO models (source, upstream_id, display_name, enabled, capabilities, last_seen_at)
             VALUES (?, ?, ?, 0, ?, ?)
             ON CONFLICT (source, upstream_id) DO UPDATE SET display_name = excluded.display_name,
                 capabilities = excluded.capabilities, last_seen_at = excluded.last_seen_at",
        )
        .bind(&source)
        .bind(&model.id)
        .bind(&model.display_name)
        .bind(model.capabilities.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    tracing::info!(
        connection = connection_id,
        models = listed.len(),
        "connection model list loaded"
    );
    Ok(listed.len())
}

async fn load(state: &AppState, connection_id: i64) -> anyhow::Result<Vec<Listed>> {
    let connection = Connection::load(state, connection_id).await?;
    let catalog = catalog::get(state).await;
    match connection.kind {
        Kind::OpenAi => {
            let list = get(state, &connection, "models").await?;
            Ok(entries(&list)
                .filter_map(|m| m["id"].as_str())
                .map(|id| Listed {
                    id: id.to_owned(),
                    display_name: None,
                    capabilities: openai_capabilities(&catalog, id),
                })
                .collect())
        }
        Kind::Anthropic => {
            let mut listed = Vec::new();
            let mut after: Option<String> = None;
            for _ in 0..MAX_PAGES {
                let path = match &after {
                    Some(id) => format!("models?limit=1000&after_id={id}"),
                    None => "models?limit=1000".to_owned(),
                };
                let page = get(state, &connection, &path).await?;
                for model in entries(&page) {
                    let Some(id) = model["id"].as_str() else { continue };
                    listed.push(Listed {
                        id: id.to_owned(),
                        display_name: model["display_name"].as_str().map(str::to_owned),
                        capabilities: anthropic_capabilities(&catalog, model),
                    });
                }
                match (page["has_more"].as_bool(), page["last_id"].as_str()) {
                    (Some(true), Some(last)) => after = Some(last.to_owned()),
                    _ => break,
                }
            }
            Ok(listed)
        }
        Kind::OpenRouter => {
            let list = get(state, &connection, "models").await?;
            Ok(entries(&list)
                .filter_map(|model| {
                    Some(Listed {
                        id: model["id"].as_str()?.to_owned(),
                        display_name: model["name"].as_str().map(str::to_owned),
                        capabilities: openrouter_capabilities(model),
                    })
                })
                .collect())
        }
    }
}

async fn get(state: &AppState, connection: &Connection, path: &str) -> anyhow::Result<Value> {
    let response = connection
        .request(state, reqwest::Method::GET, path, None)?
        .timeout(TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    // A custom base URL can answer anything; the answer is read with a size limit.
    let body = read_limited(response, MAX_LIST_BYTES).await?;
    if !status.is_success() {
        let text: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        bail!("the model list request failed with {status}: {text}");
    }
    Ok(serde_json::from_slice(&body)?)
}

async fn read_limited(response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > limit {
            bail!("the model list is larger than {} MB", limit / (1024 * 1024));
        }
    }
    Ok(body)
}

fn entries(list: &Value) -> impl Iterator<Item = &Value> {
    list["data"].as_array().into_iter().flatten()
}

/// Input kinds in the gateway names (`pdf` is a `file`).
fn input_kinds<'a>(kinds: impl Iterator<Item = &'a Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for kind in kinds.filter_map(Value::as_str) {
        let kind = if kind == "pdf" { "file" } else { kind };
        if !out.iter().any(|k| k == kind) {
            out.push(kind.to_owned());
        }
    }
    out
}

/// Values that models.dev and LiteLLM both describe.
fn catalog_capabilities(catalog: &Catalog, kind: Kind, id: &str, out: &mut Map<String, Value>) {
    if let Some(entry) = catalog.models_dev(kind, id) {
        if let Some(input) = entry["modalities"]["input"].as_array() {
            out.insert("input".into(), json!(input_kinds(input.iter())));
        }
        let efforts = entry["reasoning_options"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|option| option["type"] == "effort")
            .and_then(|option| option["values"].as_array());
        if let Some(efforts) = efforts {
            out.insert("efforts".into(), json!(efforts));
        }
        out.insert("fast".into(), json!(entry["experimental"]["modes"]["fast"].is_object()));
        if let Some(context) = entry["limit"]["context"].as_i64() {
            out.insert("context_window".into(), json!(context));
        }
        if let Some(output) = entry["limit"]["output"].as_i64() {
            out.insert("max_output".into(), json!(output));
        }
    }
    if let Some(entry) = catalog.litellm(kind, id) {
        if let Some(mode) = entry["mode"].as_str() {
            out.insert("mode".into(), json!(mode));
        }
        out.entry("max_output")
            .or_insert_with(|| entry["max_output_tokens"].clone());
        // Input kinds, when models.dev did not give them.
        let flags = [
            ("supports_vision", "image"),
            ("supports_pdf_input", "file"),
            ("supports_audio_input", "audio"),
        ];
        if !out.contains_key("input") && flags.iter().any(|(flag, _)| entry[*flag].is_boolean()) {
            let mut input = vec!["text"];
            input.extend(
                flags
                    .iter()
                    .filter(|(flag, _)| entry[*flag] == true)
                    .map(|(_, kind)| *kind),
            );
            out.insert("input".into(), json!(input));
        }
        // Fast mode: Anthropic `supports_speed`, or an OpenAI priority price.
        let fast = entry["supports_speed"] == true || !entry["input_cost_per_token_priority"].is_null();
        if fast {
            out.insert("fast".into(), json!(true));
        }
    }
}

fn openai_capabilities(catalog: &Catalog, id: &str) -> Value {
    let mut out = Map::new();
    catalog_capabilities(catalog, Kind::OpenAi, id, &mut out);
    if let Some(endpoints) = catalog
        .litellm(Kind::OpenAi, id)
        .and_then(|entry| entry["supported_endpoints"].as_array())
    {
        let mut list = Vec::new();
        if endpoints.iter().any(|e| e == "/v1/chat/completions") {
            list.push("chat");
        }
        if endpoints.iter().any(|e| e == "/v1/responses") {
            list.push("responses");
        }
        out.insert("endpoints".into(), json!(list));
    }
    // Rules from the OpenAI model pages that the catalogs do not have (or have wrong).
    if id.starts_with("gpt-audio") || id.contains("-audio-preview") || id.contains("-search-preview") {
        out.insert("endpoints".into(), json!(["chat"]));
    }
    if id.starts_with("gpt-6-astra") {
        out.insert("chat_tools".into(), json!(false));
    }
    out.retain(|_, v| !v.is_null());
    Value::Object(out)
}

fn anthropic_capabilities(catalog: &Catalog, model: &Value) -> Value {
    let id = model["id"].as_str().unwrap_or_default();
    let mut out = Map::new();
    catalog_capabilities(catalog, Kind::Anthropic, id, &mut out);
    // The Anthropic list is the better source; it wins over the catalogs.
    let caps = &model["capabilities"];
    let supported = |value: &Value| value["supported"].as_bool() == Some(true);
    if caps.is_object() {
        let mut input = vec!["text"];
        if supported(&caps["image_input"]) {
            input.push("image");
        }
        if supported(&caps["pdf_input"]) {
            input.push("file");
        }
        out.insert("input".into(), json!(input));
        let efforts: Vec<&str> = ["low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .filter(|level| supported(&caps["effort"]) && supported(&caps["effort"][*level]))
            .collect();
        out.insert("efforts".into(), json!(efforts));
        out.insert(
            "thinking".into(),
            json!({
                "adaptive": supported(&caps["thinking"]["types"]["adaptive"]),
                "enabled": supported(&caps["thinking"]["types"]["enabled"]),
            }),
        );
    }
    if catalog
        .litellm(Kind::Anthropic, id)
        .is_some_and(|entry| entry["thinking_always_on"] == true)
    {
        out.insert("thinking_always_on".into(), json!(true));
    }
    // The Anthropic thinking docs: these models refuse forced tool use on every request.
    const NO_FORCED_TOOLS: [&str; 4] = [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
        "claude-mythos-5-1",
    ];
    if NO_FORCED_TOOLS.iter().any(|prefix| id.starts_with(prefix)) {
        out.insert("forced_tools_with_thinking".into(), json!(false));
    }
    for (field, key) in [("max_input_tokens", "context_window"), ("max_tokens", "max_output")] {
        if let Some(value) = model[field].as_i64().filter(|v| *v > 0) {
            out.insert(key.into(), json!(value));
        }
    }
    out.retain(|_, v| !v.is_null());
    Value::Object(out)
}

fn openrouter_capabilities(model: &Value) -> Value {
    let mut out = Map::new();
    if let Some(input) = model["architecture"]["input_modalities"].as_array() {
        out.insert("input".into(), json!(input_kinds(input.iter())));
    }
    let parameters = model["supported_parameters"].as_array();
    if parameters.is_some_and(|p| p.iter().any(|v| v == "reasoning")) {
        // `none` turns reasoning off.
        out.insert(
            "efforts".into(),
            json!(["none", "minimal", "low", "medium", "high", "xhigh"]),
        );
    }
    if let Some(context) = model["context_length"].as_i64() {
        out.insert("context_window".into(), json!(context));
    }
    if let Some(output) = model["top_provider"]["max_completion_tokens"].as_i64() {
        out.insert("max_output".into(), json!(output));
    }
    Value::Object(out)
}
