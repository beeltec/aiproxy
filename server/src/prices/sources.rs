//! The public price lists and their conversion to `Prices`. Model keys are
//! `<provider>/<model id>`: `openai/gpt-5`, `anthropic/claude-sonnet-5`, `openrouter/<vendor>/<model>`.

use std::collections::BTreeMap;

use anyhow::bail;
use serde_json::Value;

use super::{ContextTier, ImagePrice, Prices, TokenPrices};

pub const LITELLM: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const MODELS_DEV: &str = "https://models.dev/api.json";
pub const OPENROUTER: [&str; 2] = [
    "https://openrouter.ai/api/v1/models",
    "https://openrouter.ai/api/v1/embeddings/models",
];

/// The providers whose prices the gateway uses.
const PROVIDERS: [&str; 3] = ["openai", "anthropic", "openrouter"];

/// A LiteLLM list with fewer models is not the real list.
const MIN_MODELS: usize = 50;

/// Long-context limits in LiteLLM field names (`_above_272k_tokens`).
const LITELLM_LIMITS: [i64; 6] = [32, 128, 200, 256, 272, 512];

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        // OpenRouter sends prices as strings.
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite() && *n >= 0.0)
}

/// LiteLLM token prices with a field suffix (`_priority`, `_above_272k_tokens`).
fn litellm_tokens(entry: &Value, suffix: &str) -> TokenPrices {
    let field = |name: &str| number(&entry[format!("{name}{suffix}")]);
    TokenPrices {
        input_text: field("input_cost_per_token"),
        input_text_cached: field("cache_read_input_token_cost").or_else(|| field("input_cost_per_token_cache_hit")),
        input_audio: field("input_cost_per_audio_token"),
        input_audio_cached: field("cache_read_input_audio_token_cost"),
        input_image: field("input_cost_per_image_token"),
        input_image_cached: field("cache_read_input_image_token_cost"),
        cache_write_5m: field("cache_creation_input_token_cost"),
        cache_write_1h: field("cache_creation_input_token_cost_above_1hr"),
        output_text: field("output_cost_per_token"),
        output_reasoning: field("output_cost_per_reasoning_token"),
        output_audio: field("output_cost_per_audio_token"),
        output_image: field("output_cost_per_image_token"),
    }
}

fn some(prices: TokenPrices) -> Option<TokenPrices> {
    (!prices.is_empty()).then_some(prices)
}

/// The `web_search` price of OpenAI, per call. The preview tool costs the same on reasoning
/// models and more on the others.
const OPENAI_WEB_SEARCH: f64 = 0.01;

fn litellm_prices(provider: &str, entry: &Value) -> Prices {
    let search = &entry["search_context_cost_per_query"];
    let search = number(&search["search_context_size_medium"])
        .or_else(|| search.as_object().and_then(|o| o.values().find_map(number)));
    // LiteLLM has one search price per model: for OpenAI it is the price of the preview tool;
    // the `web_search` price is known only when both are the same.
    let (web_search, web_search_preview) = match provider {
        "openai" => (search.filter(|usd| (usd - OPENAI_WEB_SEARCH).abs() < 1e-12), search),
        _ => (search, None),
    };
    let context_tiers = LITELLM_LIMITS
        .iter()
        .filter_map(|limit| {
            let suffix = format!("_above_{limit}k_tokens");
            let standard = litellm_tokens(entry, &suffix);
            let priority = some(litellm_tokens(entry, &format!("{suffix}_priority")));
            let flex = some(litellm_tokens(entry, &format!("{suffix}_flex")));
            (!standard.is_empty() || priority.is_some() || flex.is_some()).then(|| ContextTier {
                above: limit * 1000,
                standard,
                priority,
                flex,
            })
        })
        .collect();
    let specific = &entry["provider_specific_entry"];
    let geo = ["us", "eu"]
        .iter()
        .filter_map(|region| number(&specific[*region]).map(|m| ((*region).to_owned(), m)))
        .collect();
    Prices {
        standard: litellm_tokens(entry, ""),
        priority: some(litellm_tokens(entry, "_priority")),
        flex: some(litellm_tokens(entry, "_flex")),
        context_tiers,
        fast_multiplier: number(&specific["fast"]),
        fast: None,
        geo,
        web_search,
        web_search_preview,
        images: Vec::new(),
        per_character: number(&entry["input_cost_per_character"]),
        per_second: number(&entry["input_cost_per_second"]),
        partial: !entry["off_peak_pricing"].is_null() || !entry["tiered_pricing"].is_null(),
        variable: false,
    }
}

/// `<quality>/<W>-x-<H>/<model>` or `<W>-x-<H>/<model>`: the price per image of an image model.
fn litellm_image_key(key: &str) -> Option<(Option<&str>, String, &str)> {
    let parts: Vec<&str> = key.split('/').collect();
    let (quality, size, model) = match parts.as_slice() {
        [quality, size, model] => (Some(*quality), *size, *model),
        [size, model] => (None, *size, *model),
        _ => return None,
    };
    let (width, height) = size.split_once("-x-")?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(width) && digits(height)).then(|| (quality, format!("{width}x{height}"), model))
}

pub fn parse_litellm(list: &Value) -> anyhow::Result<BTreeMap<String, Prices>> {
    let mut out = BTreeMap::new();
    let mut images: Vec<(String, ImagePrice)> = Vec::new();
    for (key, entry) in list.as_object().into_iter().flatten() {
        let Some(provider) = entry["litellm_provider"].as_str().filter(|p| PROVIDERS.contains(p)) else {
            continue;
        };
        if provider == "openrouter" {
            if key.starts_with("openrouter/") {
                out.insert(key.clone(), litellm_prices(provider, entry));
            }
            continue;
        }
        if let Some((quality, size, model)) = litellm_image_key(key) {
            if let Some(usd) = number(&entry["input_cost_per_image"]) {
                let price = ImagePrice {
                    quality: quality.map(str::to_owned),
                    size,
                    usd,
                };
                images.push((format!("{provider}/{model}"), price));
            }
            continue;
        }
        // Fine-tuned models and other names with a prefix are left out.
        if key.contains('/') || key.contains(':') {
            continue;
        }
        out.insert(format!("{provider}/{key}"), litellm_prices(provider, entry));
    }
    for (key, price) in images {
        out.entry(key).or_default().images.push(price);
    }
    // An answer that is not the list must not end the current prices.
    if out.len() < MIN_MODELS {
        bail!("The list has fewer than {MIN_MODELS} models.");
    }
    Ok(out)
}

/// USD per token from USD per 1M tokens, rounded to 10 digits (the division leaves float
/// noise such as `2.0000000000000002e-7`).
fn per_token(per_million: f64) -> f64 {
    let usd = per_million / 1e6;
    if usd == 0.0 {
        return 0.0;
    }
    let scale = 10f64.powi(10 - usd.abs().log10().ceil() as i32);
    (usd * scale).round() / scale
}

/// models.dev costs are USD per 1M tokens.
fn models_dev_tokens(cost: &Value) -> TokenPrices {
    let field = |name: &str| number(&cost[name]).map(per_token);
    TokenPrices {
        input_text: field("input"),
        input_text_cached: field("cache_read"),
        input_audio: field("input_audio"),
        cache_write_5m: field("cache_write"),
        output_text: field("output"),
        output_reasoning: field("reasoning"),
        output_audio: field("output_audio"),
        ..TokenPrices::default()
    }
}

pub fn parse_models_dev(list: &Value) -> anyhow::Result<BTreeMap<String, Prices>> {
    if !list["openai"]["models"].is_object() || !list["anthropic"]["models"].is_object() {
        bail!("The list has not the expected content.");
    }
    let mut out = BTreeMap::new();
    for provider in PROVIDERS {
        for (id, model) in list[provider]["models"].as_object().into_iter().flatten() {
            let cost = &model["cost"];
            if !cost.is_object() {
                continue;
            }
            let mut context_tiers: Vec<ContextTier> = cost["tiers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|tier| tier["tier"]["type"] == "context")
                .filter_map(|tier| {
                    let size = tier["tier"]["size"].as_i64()?;
                    // A size of 200001 means "above 200000".
                    let above = if size % 1000 == 1 { size - 1 } else { size };
                    Some(ContextTier {
                        above,
                        standard: models_dev_tokens(tier),
                        ..ContextTier::default()
                    })
                })
                .collect();
            if context_tiers.is_empty() && cost["context_over_200k"].is_object() {
                context_tiers.push(ContextTier {
                    above: 200_000,
                    standard: models_dev_tokens(&cost["context_over_200k"]),
                    ..ContextTier::default()
                });
            }
            let modes = &model["experimental"]["modes"];
            let mode = |name: &str| {
                modes[name]["cost"]
                    .is_object()
                    .then(|| models_dev_tokens(&modes[name]["cost"]))
            };
            // OpenAI fast mode is the priority tier; Anthropic fast mode is its own price.
            let (priority, fast) = match provider {
                "anthropic" => (mode("priority"), mode("fast")),
                _ => (mode("priority").or_else(|| mode("fast")), None),
            };
            let prices = Prices {
                standard: models_dev_tokens(cost),
                priority,
                fast,
                context_tiers,
                ..Prices::default()
            };
            out.insert(format!("{provider}/{id}"), prices);
        }
    }
    Ok(out)
}

fn openrouter_tokens(pricing: &Value) -> TokenPrices {
    let field = |name: &str| number(&pricing[name]);
    TokenPrices {
        input_text: field("prompt"),
        input_text_cached: field("input_cache_read"),
        input_audio: field("audio"),
        input_audio_cached: field("input_audio_cache"),
        cache_write_5m: field("input_cache_write"),
        cache_write_1h: field("input_cache_write_1h"),
        output_text: field("completion"),
        output_reasoning: field("internal_reasoning"),
        output_audio: field("audio_output"),
        // A price per output image token (its values match the token prices of the models).
        output_image: field("image_output"),
        ..TokenPrices::default()
    }
}

/// OpenRouter model lists (chat and embedding models). `-1` means a price that changes per
/// request; such models get a variable price, so that no other list prices them.
pub fn parse_openrouter(lists: &[Value]) -> anyhow::Result<BTreeMap<String, Prices>> {
    if lists.iter().any(|list| !list["data"].is_array())
        || lists.first().is_none_or(|l| l["data"] == Value::Array(Vec::new()))
    {
        bail!("The list has not the expected content.");
    }
    let mut out = BTreeMap::new();
    for model in lists
        .iter()
        .flat_map(|list| list["data"].as_array().into_iter().flatten())
    {
        let (Some(id), pricing) = (model["id"].as_str(), &model["pricing"]) else {
            continue;
        };
        if pricing["prompt"] == "-1" || pricing["completion"] == "-1" {
            let prices = Prices {
                variable: true,
                ..Prices::default()
            };
            out.insert(format!("openrouter/{id}"), prices);
            continue;
        }
        let mut partial = false;
        let context_tiers = pricing["overrides"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tier| match tier["min_prompt_tokens"].as_i64() {
                Some(min) => Some(ContextTier {
                    above: min - 1,
                    standard: openrouter_tokens(tier),
                    ..ContextTier::default()
                }),
                // Prices by time of day cannot be applied.
                None => {
                    partial = true;
                    None
                }
            })
            .collect();
        let prices = Prices {
            standard: openrouter_tokens(pricing),
            context_tiers,
            web_search: number(&pricing["web_search"]),
            partial,
            ..Prices::default()
        };
        out.insert(format!("openrouter/{id}"), prices);
    }
    Ok(out)
}
