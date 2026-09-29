//! Checks and routing that all client formats share.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::auth::{Admission, ApiKey};
use super::engine::{Failure, Job};
use super::routing::{self, Alias, Route, RouteError, Upstream};
use crate::connections::Kind;
use crate::state::AppState;

/// Output tokens reserved for the tokens-per-minute limit when the client sets no maximum.
pub(super) const DEFAULT_OUTPUT_RESERVE: i64 = 4_000;
const MAX_OUTPUT_RESERVE: i64 = 8_000;
const MAX_MODEL_NAME: usize = 200;
/// Upper bound for the output items that references put back into one request.
const MAX_RESOLVED_BYTES: usize = 16 * 1024 * 1024;

pub struct Prepared {
    pub job: Job,
}

/// A client request in the Responses form, with the original body.
pub struct Incoming<'a> {
    /// `responses`, `chat` or `messages`: the route and the client format.
    pub format: &'static str,
    pub body: Value,
    pub native: Value,
    pub cache_hint: Option<&'a str>,
    pub anthropic_beta: Option<&'a str>,
    pub anthropic_version: Option<&'a str>,
}

/// Validates a Responses request body, puts back the referenced output items and builds the job.
pub async fn prepare(
    state: &AppState,
    key: &ApiKey,
    incoming: Incoming<'_>,
    permits: Admission,
) -> Result<Prepared, Failure> {
    let Incoming {
        format,
        mut body,
        mut native,
        cache_hint,
        anthropic_beta,
        anthropic_version,
    } = incoming;
    let bad = |message: &str| Failure::new(StatusCode::BAD_REQUEST, "invalid_request", message);
    let requested = body["model"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| bad("The field `model` is missing."))?;
    reject_stored_state(&body)?;
    resolve_references(state, key, &mut body)?;
    resolve_references(state, key, &mut native)?;
    reject_hosted_tools(&body)?;
    check_options(&body)?;
    let route = route(state, key, &requested).await?;
    let anthropic = matches!(
        route.upstream,
        Upstream::Connection {
            kind: Kind::Anthropic,
            ..
        }
    );
    let builtin = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["type"] == "anthropic_builtin");
    if let Some(tool) = builtin.filter(|_| !(anthropic && format == "messages")) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "unsupported_tool",
            format!(
                "The tool type `{}` works only with Anthropic models. Use a function tool with an input schema.",
                tool["tool_type"].as_str().unwrap_or_default()
            ),
        ));
    }
    let preview = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| tool["type"] == "web_search_preview");
    if preview && route.upstream == Upstream::ChatGpt {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "unsupported_tool",
            "ChatGPT models do not support `web_search_preview`. Use `web_search`.",
        ));
    }
    let image_tool = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| tool["type"] == "image_generation");
    let openai_like = matches!(
        route.upstream,
        Upstream::ChatGpt | Upstream::Connection { kind: Kind::OpenAi, .. }
    );
    if image_tool && !openai_like {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "unsupported_tool",
            "The image-generation tool works only with ChatGPT and OpenAI models.",
        ));
    }
    // Anthropic can block search domains; the other upstreams cannot.
    let blocked_domains = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| tool["type"] == "web_search" && !tool["blocked_domains"].is_null());
    if blocked_domains && !(anthropic && format == "messages") {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "unsupported_tool",
            "Web search with `blocked_domains` works only with Anthropic models. Use `allowed_domains`.",
        ));
    }
    check_inputs(&body, &route.capabilities, &route.qualified)?;
    if let Some(alias) = &route.alias {
        apply_alias(&mut body, alias, format);
    }
    // Anthropic efforts have other names; the Anthropic encoder maps them itself, and `none`
    // must stay to turn thinking off.
    if !anthropic {
        clamp_effort(&mut body, &route.capabilities);
    }

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
            native,
            anthropic_beta: anthropic_beta.map(str::to_owned),
            anthropic_version: anthropic_version.filter(|v| is_api_version(v)).map(str::to_owned),
            route_name: format,
            client_format: format,
            stream,
            cache_key,
            reserved_tokens,
            _permits: permits,
        },
    })
}

/// An Anthropic API version such as `2023-06-01`.
pub(super) fn is_api_version(version: &str) -> bool {
    version.len() == 10 && version.chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// A body that is not valid JSON, as an error in the format of the client.
pub fn bad_json(rejection: &axum::extract::rejection::JsonRejection) -> Failure {
    Failure::new(rejection.status(), "invalid_request", rejection.body_text())
}

/// Finds the enabled model for a name and checks that the key may use it.
pub async fn route(state: &AppState, key: &ApiKey, requested: &str) -> Result<Route, Failure> {
    if requested.len() > MAX_MODEL_NAME {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "The model name is too long.",
        ));
    }
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
    if !routing::allowed(&key.allowlist, &route.qualified, &route.bare_names()) {
        state.rejected.count(super::rejected::Reason::NotAllowed);
        return Err(Failure::new(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            format!("This API key may not use `{}`.", route.qualified),
        ));
    }
    Ok(route)
}

/// Alias defaults for values that the client did not set. Chat and Messages clients have no
/// summary setting (their converters only put `auto`), so there the alias summary always wins.
fn apply_alias(body: &mut Value, alias: &Alias, format: &str) {
    if let Some(effort) = &alias.effort
        && body["reasoning"]["effort"].is_null()
    {
        body["reasoning"]["effort"] = json!(effort);
    }
    if let Some(summary) = &alias.summary
        && (body["reasoning"]["summary"].is_null() || format != "responses")
    {
        body["reasoning"]["summary"] = json!(summary);
    }
    if alias.fast && body["service_tier"].is_null() {
        body["service_tier"] = json!("priority");
    }
}

const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const SERVICE_TIERS: [&str; 6] = ["auto", "default", "flex", "scale", "priority", "fast"];

/// Only known values reach the backend and the usage rows.
fn check_options(body: &Value) -> Result<(), Failure> {
    for field in ["reasoning", "text"] {
        if !body[field].is_null() && !body[field].is_object() {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("`{field}` must be an object."),
            ));
        }
    }
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
/// `web_search_preview` works only on OpenAI connections, `image_generation` only on ChatGPT
/// and OpenAI (checked after routing).
const HOSTED_TOOLS: [&str; 6] = [
    "function",
    "custom",
    "web_search",
    "web_search_preview",
    "image_generation",
    "anthropic_builtin",
];

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
/// be shared by all gateway keys through one account, so it is not supported. Top-level
/// references to earlier output items are resolved from memory (see `resolve_references`).
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
    let mut stored_reference = body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| [&item["content"], &item["output"]])
        .filter_map(Value::as_array)
        .flatten()
        .any(is_reference);
    visit_parts(&body["input"], &mut |part| {
        let kind = part["type"].as_str().unwrap_or_default();
        let provider_object = (!part["file_id"].is_null() && kind.starts_with("input_"))
            || !part["container_id"].is_null()
            || kind.starts_with("code_interpreter")
            || kind.starts_with("file_search")
            || kind.starts_with("mcp");
        if provider_object {
            stored_reference = true;
        }
    });
    // The image-generation tool can take its mask as a stored file.
    let stored_mask = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| !tool["input_image_mask"]["file_id"].is_null());
    if stored_reference || stored_mask {
        return Err(bad(
            "References to stored items or files are not supported. Send the content inline.",
        ));
    }
    Ok(())
}

/// An `item_reference`, or an item with only an `id` (its `type` is optional).
fn is_reference(item: &Value) -> bool {
    item["type"] == "item_reference" || (item["type"].is_null() && item["role"].is_null() && !item["id"].is_null())
}

/// Replaces each top-level reference in `input` with the output item that it refers to. Only
/// items from earlier answers to the same API key are known (see `item_cache`).
fn resolve_references(state: &AppState, key: &ApiKey, body: &mut Value) -> Result<(), Failure> {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let mut resolved_bytes = 0usize;
    for item in items.iter_mut().filter(|item| is_reference(item)) {
        let id = item["id"].as_str().unwrap_or_default().to_owned();
        let Some((cached, size)) = state.item_cache.get(key.id, &id) else {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("The item `{id}` is unknown or has expired. Send the item content inline."),
            ));
        };
        // A client can repeat one reference many times, so the total is bounded.
        resolved_bytes += size;
        if resolved_bytes > MAX_RESOLVED_BYTES {
            return Err(Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "The referenced items are larger than 16 MB. Send fewer references or the content inline.",
            ));
        }
        *item = cached.as_ref().clone();
    }
    Ok(())
}

/// Refuses inputs that the model cannot read (when its input kinds are known). An empty list is
/// unknown too: the backend then decides.
fn check_inputs(body: &Value, capabilities: &Value, model: &str) -> Result<(), Failure> {
    let Some(known) = capabilities["input"].as_array().filter(|list| !list.is_empty()) else {
        return Ok(());
    };
    let supports = |kind: &str| known.iter().any(|k| k == kind);
    let mut missing = None;
    visit_parts(&body["input"], &mut |part| {
        let needed = match part["type"].as_str().unwrap_or_default() {
            "input_image" => "image",
            "input_file" => "file",
            "input_audio" => "audio",
            "input_video" => "video",
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
