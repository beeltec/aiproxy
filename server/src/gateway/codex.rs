//! Requests to the ChatGPT Codex backend (`/responses`).

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Map, Value, json};

use crate::chatgpt::{accounts, backend};
use crate::state::AppState;

/// Fields that the backend accepts. It answers other fields with 400.
const ALLOWED_FIELDS: [&str; 12] = [
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "text",
    "service_tier",
    "prompt_cache_key",
    "include",
    "client_metadata",
];
const HEADERS_TIMEOUT: Duration = Duration::from_secs(120);

/// Builds the backend body from a Responses request: only allowed fields, a list as `input`,
/// no stored state, always streamed, and encrypted reasoning for the next turn.
pub fn backend_body(request: &Value, upstream_model: &str, cache_key: &str) -> Value {
    let mut body = Map::new();
    if let Some(object) = request.as_object() {
        for field in ALLOWED_FIELDS {
            if let Some(value) = object.get(field).filter(|v| !v.is_null()) {
                body.insert(field.to_owned(), value.clone());
            }
        }
    }
    body.insert("model".into(), json!(upstream_model));
    if let Some(Value::String(text)) = body.get("input") {
        let message =
            json!([{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] }]);
        body.insert("input".into(), message);
    }
    if body
        .get("instructions")
        .is_some_and(|i| i.as_str().is_some_and(|s| s.trim().is_empty()))
    {
        body.remove("instructions");
    }
    // The backend accepts a request without instructions, but it refuses an empty text.

    body.entry("tools").or_insert_with(|| json!([]));
    body.entry("tool_choice").or_insert_with(|| json!("auto"));
    body.entry("parallel_tool_calls").or_insert(json!(true));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    let mut include: Vec<Value> = body
        .get("include")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut wanted = vec!["reasoning.encrypted_content"];
    // Without this, the backend does not return the pages that a web search found.
    let searches = body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t["type"].as_str().is_some_and(|k| k.starts_with("web_search")))
    });
    if searches {
        wanted.push("web_search_call.action.sources");
    }
    for value in wanted {
        if !include.iter().any(|v| v == value) {
            include.push(json!(value));
        }
    }
    body.insert("include".into(), Value::Array(include));
    body.entry("prompt_cache_key").or_insert_with(|| json!(cache_key));
    // Fast mode is called "priority" at the backend.
    if body.get("service_tier").and_then(Value::as_str) == Some("fast") {
        body.insert("service_tier".into(), json!("priority"));
    }
    if body
        .get("service_tier")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "auto" || t == "default")
    {
        body.remove("service_tier");
    }
    Value::Object(body)
}

/// The Codex CLI tells the backend the model and tier in this header.
fn routing_hint(body: &Value) -> String {
    let model = body["model"].as_str().unwrap_or_default();
    match body["service_tier"].as_str() {
        Some(tier) => format!("model={model};tier={tier}"),
        None => format!("model={model}"),
    }
}

pub enum SendError {
    /// Access token not accepted: refresh and try once more.
    Unauthorized,
    /// Usage limit reached, blocked until this unix time. Without a reset time, the stored quota gives it.
    UsageLimit { until: Option<i64>, message: String },
    /// Short-term throttling: the account is not blocked.
    Throttled { retry_after: Option<i64>, message: String },
    /// Any other failure, with the status to show the client.
    Failed { status: u16, message: String },
}

/// Sends the request. On success the response is an SSE stream.
pub async fn send(state: &AppState, account: i64, body: &Value) -> Result<reqwest::Response, SendError> {
    let credentials = accounts::credentials(state, account)
        .await
        .map_err(|err| SendError::Failed {
            status: 500,
            message: format!("cannot read the account tokens: {err}"),
        })?;
    let installation_id = backend::installation_id(&state.db)
        .await
        .map_err(|err| SendError::Failed {
            status: 500,
            message: format!("database error: {err}"),
        })?;
    tracing::debug!(service_tier = ?body.get("service_tier"), model = ?body.get("model"), "backend request");
    let request = state
        .http
        .post(format!("{}/responses", backend::BASE_URL))
        .header("Accept", "text/event-stream")
        .header("OpenAI-Beta", "responses=experimental")
        .header("session-id", body["prompt_cache_key"].as_str().unwrap_or_default())
        .header("x-codex-routing-hint", routing_hint(body))
        .json(body);
    let send = backend::with_headers(request, &credentials, &installation_id).send();
    let response = tokio::time::timeout(HEADERS_TIMEOUT, send)
        .await
        .map_err(|_| SendError::Failed {
            status: 504,
            message: "the ChatGPT backend did not answer in time".into(),
        })?
        .map_err(|err| SendError::Failed {
            status: 502,
            message: format!("cannot reach the ChatGPT backend: {err}"),
        })?;
    crate::chatgpt::select::store_quota(state, account, response.headers()).await;

    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let text = error_text(response).await;
    Err(classify(status.as_u16(), &text, retry_after))
}

/// Reads at most 1 MB of an error body, within 30 s.
async fn error_text(response: reqwest::Response) -> String {
    const LIMIT: usize = 1024 * 1024;
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    let read = async {
        while let Some(Ok(chunk)) = chunks.next().await {
            body.extend_from_slice(&chunk);
            if body.len() >= LIMIT {
                body.truncate(LIMIT);
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(30), read).await;
    String::from_utf8_lossy(&body).into_owned()
}

/// Maps an error answer (HTTP body or a `response.failed` event) to a `SendError`.
pub fn classify(status: u16, body: &str, retry_after: Option<i64>) -> SendError {
    let json: Value = serde_json::from_str(body).unwrap_or_default();
    let error = if json["error"].is_object() {
        &json["error"]
    } else {
        &json
    };
    // Some errors have the generic type "error" and the real kind in `code`.
    let kind = error["type"]
        .as_str()
        .filter(|kind| *kind != "error")
        .or_else(|| error["code"].as_str())
        .unwrap_or_default();
    let message = error["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| body.chars().take(300).collect());
    if status == 401 || matches!(kind, "invalid_api_key" | "token_expired" | "authentication_error") {
        return SendError::Unauthorized;
    }
    if matches!(kind, "usage_limit_reached" | "usage_not_included") {
        let now = crate::db::now();
        let until = error["resets_at"]
            .as_i64()
            .or_else(|| error["resets_in_seconds"].as_i64().map(|s| now + s))
            .or_else(|| retry_after.map(|s| now + s));
        return SendError::UsageLimit { until, message };
    }
    if kind == "rate_limit_exceeded" || status == 429 {
        return SendError::Throttled { retry_after, message };
    }
    SendError::Failed { status, message }
}
