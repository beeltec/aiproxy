//! Checks and routing that all client formats share.

use axum::http::StatusCode;
use serde_json::Value;

use super::auth::{Admission, ApiKey};
use super::engine::{Failure, Job};
use super::routing::{self, Route, RouteError};
use crate::state::AppState;

/// Output tokens reserved for the tokens-per-minute limit when the client sets no maximum.
const DEFAULT_OUTPUT_RESERVE: i64 = 4_000;
const MAX_OUTPUT_RESERVE: i64 = 8_000;

pub struct Prepared {
    pub job: Job,
}

/// Validates a Responses request body and builds the job.
pub async fn prepare(
    state: &AppState,
    key: &ApiKey,
    mut body: Value,
    route_name: &'static str,
    client_format: &'static str,
    cache_hint: Option<&str>,
    permits: Admission,
) -> Result<Prepared, Failure> {
    let bad = |message: &str| Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message);
    let requested = body["model"]
        .as_str()
        .ok_or_else(|| bad("The field `model` is missing."))?;
    reject_stored_state(&body)?;
    reject_hosted_tools(&body)?;
    check_options(&body)?;
    let route = route(state, key, requested).await?;
    check_inputs(&body, &route.capabilities, &route.qualified)?;
    clamp_effort(&mut body, &route.capabilities);

    let output_reserve = body["max_output_tokens"]
        .as_i64()
        .unwrap_or(DEFAULT_OUTPUT_RESERVE)
        .clamp(1, MAX_OUTPUT_RESERVE);
    let estimate = crate::tokens::estimate(&body).await as i64 + output_reserve;
    let reserved_tokens = match state.key_limits.reserve_tokens(key.id, estimate) {
        Ok(reserved) => reserved,
        Err(wait) => {
            state.rejected.count(super::rejected::Reason::RateLimited);
            let mut failure = Failure::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                "This key used its tokens per minute.",
            );
            failure.retry_after = Some(wait);
            return Err(failure);
        }
    };

    let stream = body["stream"].as_bool().unwrap_or(false);
    // The same key for the same conversation start lets the backend reuse its prompt cache.
    let cache_key = cache_hint
        .map(str::to_owned)
        .or_else(|| body["prompt_cache_key"].as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            let first = crate::crypto::sha256(
                format!("{}:{}", key.id, body["instructions"].as_str().unwrap_or_default()).as_bytes(),
            );
            first.iter().take(16).map(|b| format!("{b:02x}")).collect()
        });
    Ok(Prepared {
        job: Job {
            key: key.clone(),
            route,
            body,
            route_name,
            client_format,
            stream,
            cache_key,
            reserved_tokens,
            _permits: permits,
        },
    })
}

/// A body that is not valid JSON, as an error in the format of the client.
pub fn bad_json(rejection: &axum::extract::rejection::JsonRejection) -> Failure {
    Failure::new(rejection.status(), "invalid_request", rejection.body_text())
}

/// Finds the enabled model for a name and checks that the key may use it.
pub async fn route(state: &AppState, key: &ApiKey, requested: &str) -> Result<Route, Failure> {
    let route = match routing::resolve(&state.db, requested).await {
        Ok(route) => route,
        Err(RouteError::NotFound(name)) => {
            return Err(Failure::new(
                StatusCode::NOT_FOUND,
                "model_not_found",
                format!("The model `{name}` does not exist or is not enabled."),
            ));
        }
        Err(RouteError::Ambiguous(name, options)) => {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("`{name}` matches several models. Use one of: {}.", options.join(", ")),
            ));
        }
        Err(RouteError::Db(err)) => {
            tracing::error!(error = %err, "database error in routing");
            return Err(Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Internal error.",
            ));
        }
    };
    if !routing::allowed(&key.allowlist, &route) {
        state.rejected.count(super::rejected::Reason::NotAllowed);
        return Err(Failure::new(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            format!("This API key may not use `{}`.", route.qualified),
        ));
    }
    Ok(route)
}

const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const SERVICE_TIERS: [&str; 6] = ["auto", "default", "flex", "scale", "priority", "fast"];

/// Only known values reach the backend and the usage rows.
fn check_options(body: &Value) -> Result<(), Failure> {
    let known = |value: &Value, list: &[&str]| value.is_null() || value.as_str().is_some_and(|v| list.contains(&v));
    if !known(&body["reasoning"]["effort"], &EFFORTS) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("Unknown reasoning effort. Use one of: {}.", EFFORTS.join(", ")),
        ));
    }
    if !known(&body["service_tier"], &SERVICE_TIERS) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("Unknown service tier. Use one of: {}.", SERVICE_TIERS.join(", ")),
        ));
    }
    Ok(())
}

/// An effort that the model does not support becomes the nearest supported one.
fn clamp_effort(body: &mut Value, capabilities: &Value) {
    let Some(supported) = capabilities["efforts"].as_array().filter(|list| !list.is_empty()) else {
        return;
    };
    let Some(requested) = body["reasoning"]["effort"].as_str() else {
        return;
    };
    if supported.iter().any(|e| e == requested) {
        return;
    }
    let rank = |effort: &str| EFFORTS.iter().position(|e| *e == effort);
    let Some(wanted) = rank(requested) else {
        return;
    };
    let nearest = supported
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|e| rank(e).map(|r| (r.abs_diff(wanted), std::cmp::Reverse(r), e)))
        .min()
        .map(|(_, _, e)| e.to_owned());
    if let Some(nearest) = nearest {
        tracing::debug!(requested, %nearest, "reasoning effort changed to a supported value");
        body["reasoning"]["effort"] = Value::String(nearest);
    }
}

/// Hosted tools that the gateway allows. The others can reach provider-side objects (files,
/// containers, connectors) through the shared account, and their charges are not tracked.
const HOSTED_TOOLS: [&str; 4] = ["function", "custom", "web_search", "web_search_preview"];

fn reject_hosted_tools(body: &Value) -> Result<(), Failure> {
    for tool in body["tools"].as_array().into_iter().flatten() {
        let kind = tool["type"].as_str().unwrap_or_default();
        if !HOSTED_TOOLS.contains(&kind) {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "unsupported_tool",
                format!("The tool type `{kind}` is not supported. Use function tools or web search."),
            ));
        }
    }
    Ok(())
}

/// Server-side state (stored responses, conversations, provider files, background runs) would
/// be shared by all gateway keys through one account, so it is not supported.
fn reject_stored_state(body: &Value) -> Result<(), Failure> {
    let bad = |message: &str| Failure::new(StatusCode::BAD_REQUEST, "unsupported_parameter", message);
    if !body["previous_response_id"].is_null() || !body["conversation"].is_null() {
        return Err(bad(
            "`previous_response_id` and `conversation` are not supported. Send the full conversation in `input`.",
        ));
    }
    if body["background"].as_bool() == Some(true) {
        return Err(bad("`background` is not supported."));
    }
    if body["prompt"]["id"].is_string() {
        return Err(bad("Stored prompts (`prompt.id`) are not supported."));
    }
    let mut stored_reference = false;
    visit_parts(&body["input"], &mut |part| {
        let kind = part["type"].as_str().unwrap_or_default();
        let provider_object = kind == "item_reference"
            || (!part["file_id"].is_null() && kind.starts_with("input_"))
            || !part["container_id"].is_null()
            || kind.starts_with("code_interpreter")
            || kind.starts_with("file_search")
            || kind.starts_with("mcp");
        if provider_object {
            stored_reference = true;
        }
    });
    if stored_reference {
        return Err(bad(
            "References to stored items or files are not supported. Send the content inline.",
        ));
    }
    Ok(())
}

/// Refuses inputs that the model cannot read (when its input kinds are known).
fn check_inputs(body: &Value, capabilities: &Value, model: &str) -> Result<(), Failure> {
    let Some(known) = capabilities["input"].as_array() else {
        return Ok(());
    };
    let supports = |kind: &str| known.iter().any(|k| k == kind);
    let mut missing = None;
    visit_parts(&body["input"], &mut |part| {
        let needed = match part["type"].as_str().unwrap_or_default() {
            "input_image" => "image",
            "input_file" => "file",
            "input_audio" => "audio",
            _ => return,
        };
        if !supports(needed) && missing.is_none() {
            missing = Some(needed);
        }
    });
    match missing {
        Some(kind) => Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "unsupported_input",
            format!("`{model}` cannot read {kind} input."),
        )),
        None => Ok(()),
    }
}

/// Calls `visit` for every input item and every content part.
fn visit_parts(input: &Value, visit: &mut impl FnMut(&Value)) {
    let Some(items) = input.as_array() else { return };
    for item in items {
        visit(item);
        if let Some(parts) = item["content"].as_array() {
            parts.iter().for_each(&mut *visit);
        }
        if let Some(parts) = item["output"].as_array() {
            parts.iter().for_each(&mut *visit);
        }
    }
}
