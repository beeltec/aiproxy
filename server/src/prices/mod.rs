//! Prices from public price lists and admin overrides, and the cost ("API value") of usage rows.

pub mod recompute;
pub mod sources;
pub mod sync;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::usage::Tokens;

/// USD per token. An empty field has no price.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenPrices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_text: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_text_cached: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_audio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_audio_cached: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_image: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_image_cached: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_text: Option<f64>,
    /// Empty: the output text price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reasoning: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_audio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_image: Option<f64>,
}

impl TokenPrices {
    fn get(&self, category: usize) -> Option<f64> {
        match category {
            0 => self.input_text,
            1 => self.input_text_cached,
            2 => self.input_audio,
            3 => self.input_audio_cached,
            4 => self.input_image,
            5 => self.input_image_cached,
            6 => self.cache_write_5m,
            7 => self.cache_write_1h,
            8 => self.output_text,
            9 => self.output_reasoning,
            10 => self.output_audio,
            _ => self.output_image,
        }
    }

    /// The prices as listed (reasoning without the output price).
    fn values(&self) -> [Option<f64>; 12] {
        std::array::from_fn(|category| self.get(category))
    }

    /// The prices with the output price for reasoning when it has none.
    fn effective(&self) -> [Option<f64>; 12] {
        let mut values = self.values();
        values[9] = values[9].or(values[8]);
        values
    }

    pub fn is_empty(&self) -> bool {
        self.values().iter().all(Option::is_none)
    }
}

/// Prices for long requests: they apply when the input of one request is above `above` tokens.
/// An empty field keeps the price below the limit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextTier {
    pub above: i64,
    pub standard: TokenPrices,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TokenPrices>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex: Option<TokenPrices>,
}

/// The price of one generated image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagePrice {
    /// Empty: all qualities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// For example `1024x1024`.
    pub size: String,
    pub usd: f64,
}

/// All prices of one model from one source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Prices {
    pub standard: TokenPrices,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TokenPrices>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex: Option<TokenPrices>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_tiers: Vec<ContextTier>,
    /// Anthropic fast mode: the standard prices times this number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_multiplier: Option<f64>,
    /// Anthropic fast mode prices (used before the multiplier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<TokenPrices>,
    /// Anthropic inference geography (`us`) → multiplier for all token prices.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub geo: BTreeMap<String, f64>,
    /// USD per call of the `web_search` tool (also Anthropic web search).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<f64>,
    /// USD per call of the `web_search_preview` tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search_preview: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePrice>,
    /// USD per input character (speech).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_character: Option<f64>,
    /// USD per second of input audio (transcription).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_second: Option<f64>,
    /// The source has a price rule that the gateway cannot apply (for example off-peak
    /// prices), so costs from it are not complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// The price changes per request (OpenRouter routers): the cost is unknown, and no other
    /// list is used for the model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub variable: bool,
}

impl Prices {
    /// A context tier that raises the output price and does not list reasoning raises the
    /// reasoning price too, when the base prices reasoning like output (OpenRouter and LiteLLM
    /// list reasoning only in the base prices).
    fn tie_reasoning(&mut self) {
        let tied = |base: &TokenPrices, tier: &mut TokenPrices| {
            if base.output_reasoning.is_some()
                && base.output_reasoning == base.output_text
                && tier.output_reasoning.is_none()
                && tier.output_text.is_some()
            {
                tier.output_reasoning = tier.output_text;
            }
        };
        for tier in &mut self.context_tiers {
            tied(&self.standard, &mut tier.standard);
            if let (Some(base), Some(tier)) = (&self.priority, &mut tier.priority) {
                tied(base, tier);
            }
            if let (Some(base), Some(tier)) = (&self.flex, &mut tier.flex) {
                tied(base, tier);
            }
        }
    }

    /// Returns a readable message for the first invalid price.
    pub fn validate(&self) -> Result<(), String> {
        let mut numbers: Vec<f64> = Vec::new();
        let mut add = |prices: &TokenPrices| numbers.extend(prices.values().into_iter().flatten());
        add(&self.standard);
        self.priority.iter().for_each(&mut add);
        self.flex.iter().for_each(&mut add);
        self.fast.iter().for_each(&mut add);
        for tier in &self.context_tiers {
            add(&tier.standard);
            tier.priority.iter().for_each(&mut add);
            tier.flex.iter().for_each(&mut add);
            if tier.above < 0 {
                return Err("A context tier limit must not be negative.".into());
            }
        }
        numbers.extend(self.fast_multiplier);
        numbers.extend(self.geo.values());
        numbers.extend(self.web_search);
        numbers.extend(self.web_search_preview);
        numbers.extend(self.images.iter().map(|image| image.usd));
        numbers.extend(self.per_character);
        numbers.extend(self.per_second);
        if numbers.iter().any(|n| !n.is_finite() || *n < 0.0) {
            return Err("Prices must be numbers of 0 or more.".into());
        }
        Ok(())
    }
}

/// The usage column names of the cost categories, in the order of `TokenPrices::get` and then
/// the other quantities.
pub const CATEGORIES: [&str; 17] = [
    "input_text",
    "input_text_cached",
    "input_audio",
    "input_audio_cached",
    "input_image",
    "input_image_cached",
    "cache_write_5m",
    "cache_write_1h",
    "output_text",
    "output_reasoning",
    "output_audio",
    "output_image",
    "web_search_calls",
    "web_search_preview_calls",
    "images_generated",
    "characters",
    "seconds",
];

/// What the cost of a usage row depends on.
#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub struct CostInput {
    pub component: String,
    pub upstream: String,
    pub resolved_model: Option<String>,
    pub usage_status: String,
    pub usage_exact: bool,
    pub service_tier_reported: Option<String>,
    pub speed: Option<String>,
    pub inference_geo: Option<String>,
    pub input_text: i64,
    pub input_text_cached: i64,
    pub input_audio: i64,
    pub input_audio_cached: i64,
    pub input_image: i64,
    pub input_image_cached: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub output_text: i64,
    pub output_reasoning: i64,
    pub output_audio: i64,
    pub output_image: i64,
    pub web_search_calls: i64,
    pub web_search_preview_calls: i64,
    pub images_generated: i64,
    pub image_size: Option<String>,
    pub image_quality: Option<String>,
    pub characters: i64,
    pub seconds: Option<f64>,
}

/// The columns of `CostInput`, for queries.
pub const COST_INPUT_COLUMNS: &str = "component, upstream, resolved_model, usage_status, usage_exact,
    service_tier_reported, speed, inference_geo, input_text, input_text_cached, input_audio,
    input_audio_cached, input_image, input_image_cached, cache_write_5m, cache_write_1h, output_text,
    output_reasoning, output_audio, output_image, web_search_calls, web_search_preview_calls,
    images_generated, image_size, image_quality, characters, seconds";

impl CostInput {
    pub fn set_tokens(&mut self, tokens: &Tokens) {
        self.input_text = tokens.input_text;
        self.input_text_cached = tokens.input_text_cached;
        self.input_audio = tokens.input_audio;
        self.input_audio_cached = tokens.input_audio_cached;
        self.input_image = tokens.input_image;
        self.input_image_cached = tokens.input_image_cached;
        self.cache_write_5m = tokens.cache_write_5m;
        self.cache_write_1h = tokens.cache_write_1h;
        self.output_text = tokens.output_text;
        self.output_reasoning = tokens.output_reasoning;
        self.output_audio = tokens.output_audio;
        self.output_image = tokens.output_image;
    }

    fn tokens(&self) -> [i64; 12] {
        [
            self.input_text,
            self.input_text_cached,
            self.input_audio,
            self.input_audio_cached,
            self.input_image,
            self.input_image_cached,
            self.cache_write_5m,
            self.cache_write_1h,
            self.output_text,
            self.output_reasoning,
            self.output_audio,
            self.output_image,
        ]
    }

    /// The provider kind (`openai`, `anthropic`, `openrouter`) and the model id for the price
    /// lists. ChatGPT models and the image tool have OpenAI API prices.
    fn priced_model(&self) -> Option<(&str, &str)> {
        let resolved = self.resolved_model.as_deref()?;
        if self.component == "image_tool" {
            return Some(("openai", resolved));
        }
        // `<connection or chatgpt>/<model>`; a connection name has no slash.
        let (_, model) = resolved.split_once('/')?;
        let kind = if self.upstream == "chatgpt" {
            "openai"
        } else {
            self.upstream.as_str()
        };
        Some((kind, model))
    }
}

/// The cost of a usage row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cost {
    /// `None` when no price is known.
    pub nano: Option<i64>,
    pub complete: bool,
    /// Nano-USD per category (only categories with a cost).
    pub parts: BTreeMap<&'static str, i64>,
    pub version: Option<i64>,
}

impl Cost {
    pub fn parts_json(&self) -> Option<String> {
        self.nano
            .map(|_| serde_json::to_string(&self.parts).expect("cost parts serialize"))
    }
}

/// The current price versions and the active overrides, in memory.
#[derive(Default)]
pub struct PriceBook {
    /// (source, model key) → (version id, prices)
    versions: HashMap<(String, String), (i64, Arc<Prices>)>,
    /// match key → (version id, prices)
    overrides: HashMap<String, (i64, Arc<Prices>)>,
}

/// The price book, replaced after a sync or an override change.
#[derive(Clone, Default)]
pub struct PriceCache {
    book: Arc<RwLock<Arc<PriceBook>>>,
    /// One reload at a time, so an older snapshot cannot replace a newer one.
    reloading: Arc<tokio::sync::Mutex<()>>,
}

impl PriceCache {
    pub fn get(&self) -> Arc<PriceBook> {
        self.book.read().expect("price cache lock").clone()
    }

    /// Loads the price book from the database.
    pub async fn reload(&self, db: &SqlitePool) -> Result<(), sqlx::Error> {
        let _reloading = self.reloading.lock().await;
        let rows: Vec<(i64, String, String, String)> =
            sqlx::query_as("SELECT id, source, model_key, prices FROM price_versions WHERE current = 1")
                .fetch_all(db)
                .await?;
        let mut book = PriceBook::default();
        for (id, source, key, prices) in rows {
            match serde_json::from_str::<Prices>(&prices) {
                Ok(mut prices) => {
                    prices.tie_reasoning();
                    book.versions.insert((source, key), (id, Arc::new(prices)));
                }
                Err(err) => tracing::error!(version = id, error = %err, "cannot read a price version"),
            }
        }
        let overrides: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT o.match_key, v.id, v.prices FROM price_overrides o
             JOIN price_versions v ON v.id = o.version_id WHERE o.active = 1",
        )
        .fetch_all(db)
        .await?;
        for (key, id, prices) in overrides {
            match serde_json::from_str::<Prices>(&prices) {
                Ok(mut prices) => {
                    prices.tie_reasoning();
                    book.overrides.insert(key, (id, Arc::new(prices)));
                }
                Err(err) => tracing::error!(version = id, error = %err, "cannot read an override"),
            }
        }
        *self.book.write().expect("price cache lock") = Arc::new(book);
        Ok(())
    }
}

/// Where a price came from.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Resolved {
    pub version: i64,
    /// `override`, `openrouter`, `litellm` or `models_dev`
    pub source: String,
    /// The override match key or the model key of the source.
    pub key: String,
    #[serde(skip)]
    pub prices: Arc<Prices>,
}

impl PriceBook {
    /// The prices of a model: an override for the client model name, an override for the model
    /// key, the provider's own list (OpenRouter), LiteLLM, models.dev. The first found is used as
    /// a whole. A model id without its date (or `-codex`) is tried when the full id has none.
    pub fn resolve(&self, client_model: Option<&str>, kind: &str, model: &str) -> Option<Resolved> {
        let found = |source: &str, key: String| {
            let entry = if source == "override" {
                self.overrides.get(&key)
            } else {
                self.versions.get(&(source.to_owned(), key.clone()))
            };
            entry.map(|(version, prices)| Resolved {
                version: *version,
                source: source.to_owned(),
                key,
                prices: prices.clone(),
            })
        };
        if let Some(resolved) = client_model.and_then(|name| found("override", name.to_owned())) {
            return Some(resolved);
        }
        let mut names = vec![model.to_owned()];
        names.extend(undated(model));
        for name in names {
            let key = format!("{kind}/{name}");
            let mut sources = vec!["override"];
            if kind == "openrouter" {
                sources.push("openrouter");
            }
            sources.extend(["litellm", "models_dev"]);
            for source in sources {
                if let Some(resolved) = found(source, key.clone()) {
                    return Some(resolved);
                }
            }
        }
        None
    }

    /// The price of a usage row, with its cost.
    pub fn cost(&self, input: &CostInput) -> Cost {
        let resolved = input.priced_model().and_then(|(kind, model)| {
            let client = (input.component != "image_tool")
                .then_some(input.resolved_model.as_deref())
                .flatten();
            self.resolve(client, kind, model)
        });
        // These models bill a fixed block of search tokens per call, which no list describes.
        let fixed_search_tokens = input.web_search_calls > 0
            && input.priced_model().is_some_and(|(kind, model)| {
                kind == "openai" && FIXED_SEARCH_TOKENS.iter().any(|m| model.starts_with(m))
            });
        match resolved {
            Some(resolved) => {
                let cost = cost(input, &resolved.prices);
                Cost {
                    version: Some(resolved.version),
                    complete: cost.complete && !fixed_search_tokens,
                    ..cost
                }
            }
            None => Cost::default(),
        }
    }
}

/// OpenAI models whose `web_search` calls add a fixed number of input tokens.
const FIXED_SEARCH_TOKENS: [&str; 2] = ["gpt-4o-mini", "gpt-4.1-mini"];

/// Model ids without a date suffix (`-2025-08-07`, `-20251001`) or a `-codex` suffix.
fn undated(model: &str) -> Option<String> {
    let bytes = model.as_bytes();
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    let n = bytes.len();
    // `-YYYY-MM-DD`
    if n > 11 && bytes[n - 11] == b'-' && bytes[n - 6] == b'-' && bytes[n - 3] == b'-' {
        let (year, month, day) = (&bytes[n - 10..n - 6], &bytes[n - 5..n - 3], &bytes[n - 2..]);
        if digits(year) && digits(month) && digits(day) {
            return Some(model[..n - 11].to_owned());
        }
    }
    if n > 9 && bytes[n - 9] == b'-' && digits(&bytes[n - 8..]) {
        return Some(model[..n - 9].to_owned());
    }
    model.strip_suffix("-codex").map(str::to_owned)
}

fn nano(usd: f64) -> i64 {
    (usd * 1e9).round() as i64
}

/// The cost of a usage row with the prices of one version.
pub fn cost(input: &CostInput, prices: &Prices) -> Cost {
    if prices.variable {
        return Cost::default();
    }
    let mut complete = input.usage_status != "estimated" && input.usage_exact && !prices.partial;
    let mut parts = BTreeMap::new();
    let tokens = input.tokens();

    // Service tier from the answer. Unknown tiers use the standard prices.
    let tier = match input.service_tier_reported.as_deref() {
        Some("priority" | "fast") => "priority",
        Some("flex") => "flex",
        None | Some("default" | "auto" | "standard") => "standard",
        Some(_) => {
            complete = false;
            "standard"
        }
    };
    let tiered = |priority: &Option<TokenPrices>,
                  flex: &Option<TokenPrices>,
                  get: fn(&TokenPrices) -> [Option<f64>; 12]| match tier {
        "priority" => priority.as_ref().map(get),
        "flex" => flex.as_ref().map(get),
        _ => None,
    };
    // Layers: the base prices, then each context tier that the input is above, from low to
    // high (the whole request uses the prices of the tiers it is above). A layer changes only
    // the prices it lists. In a layer, a tier price that is missing uses the standard price of
    // the layer; the cost is then not complete.
    let input_total: i64 = tokens[..8].iter().sum();
    let mut contexts: Vec<&ContextTier> = prices.context_tiers.iter().filter(|t| input_total > t.above).collect();
    contexts.sort_by_key(|t| t.above);
    // In the base prices, reasoning without its own price is priced like output in each
    // service tier; a context tier changes reasoning only when it lists it.
    let mut layers = vec![(
        prices.standard.effective(),
        tiered(&prices.priority, &prices.flex, TokenPrices::effective),
    )];
    layers.extend(
        contexts
            .iter()
            .map(|t| (t.standard.values(), tiered(&t.priority, &t.flex, TokenPrices::values))),
    );
    let mut effective = [None; 12];
    let mut from_standard = [false; 12];
    for (standard, tier_values) in layers {
        for category in 0..12 {
            match (tier_values.and_then(|values| values[category]), standard[category]) {
                (Some(price), _) => {
                    effective[category] = Some(price);
                    from_standard[category] = false;
                }
                (None, Some(price)) => {
                    effective[category] = Some(price);
                    from_standard[category] = tier != "standard";
                }
                (None, None) => {}
            }
        }
    }
    // Reasoning without its own price uses the output price (after the layers, so that a
    // layer changes only what it lists).
    if effective[9].is_none() {
        effective[9] = effective[8];
        from_standard[9] = from_standard[8];
    }
    // Anthropic fast mode.
    if input.speed.as_deref() == Some("fast") {
        match (&prices.fast, prices.fast_multiplier) {
            (Some(fast), _) => {
                let fast = fast.effective();
                for (category, price) in effective.iter_mut().enumerate() {
                    if tokens[category] > 0 && fast[category].is_none() {
                        complete = false;
                    }
                    *price = fast[category].or(*price);
                }
            }
            (None, Some(multiplier)) => {
                for price in effective.iter_mut().flatten() {
                    *price *= multiplier;
                }
            }
            (None, None) => complete = false,
        }
    }
    // Anthropic inference geography.
    let geo = match input.inference_geo.as_deref() {
        None | Some("" | "global" | "not_available") => 1.0,
        Some(region) => prices.geo.get(region).copied().unwrap_or_else(|| {
            complete = false;
            1.0
        }),
    };

    let used_tokens = tokens.iter().any(|n| *n > 0);
    let no_token_price = (0..12).all(|category| tokens[category] == 0 || effective[category].is_none());
    // A model priced per unit (image, character, second) without prices for the used tokens
    // uses the unit price; its tokens are then only a measure.
    let per_unit = |quantity: bool, price: bool| quantity && price && no_token_price;
    let per_image = image_price(prices, input);
    let use_images = per_unit(input.images_generated > 0, per_image.is_some());
    let use_characters = per_unit(input.characters > 0, prices.per_character.is_some());
    let seconds = input.seconds.filter(|s| *s > 0.0);
    let use_seconds = per_unit(seconds.is_some(), prices.per_second.is_some());
    if use_images {
        parts.insert(
            "images_generated",
            nano(input.images_generated as f64 * per_image.unwrap_or(0.0)),
        );
    }
    if use_characters {
        parts.insert(
            "characters",
            nano(input.characters as f64 * prices.per_character.unwrap_or(0.0)),
        );
    }
    if let Some(seconds) = seconds.filter(|_| use_seconds) {
        parts.insert("seconds", nano(seconds * prices.per_second.unwrap_or(0.0)));
    }
    if !(use_images || use_characters || use_seconds) {
        for category in 0..12 {
            if tokens[category] == 0 {
                continue;
            }
            match effective[category] {
                Some(price) => {
                    parts.insert(CATEGORIES[category], nano(tokens[category] as f64 * price * geo));
                    if from_standard[category] {
                        complete = false;
                    }
                }
                None => complete = false,
            }
        }
        // Images, characters and seconds without tokens need a unit price.
        let unpriced_units = (input.images_generated > 0 && tokens[11] == 0)
            || (!used_tokens && (input.characters > 0 || seconds.is_some()));
        if unpriced_units {
            complete = false;
        }
    }
    for (count, price, category) in [
        (input.web_search_calls, prices.web_search, "web_search_calls"),
        (
            input.web_search_preview_calls,
            prices.web_search_preview,
            "web_search_preview_calls",
        ),
    ] {
        if count == 0 {
            continue;
        }
        match price {
            Some(price) => {
                parts.insert(category, nano(count as f64 * price));
            }
            None => complete = false,
        }
    }
    // Usage without any price that applies has an unknown cost (a price of 0 is a price).
    let used = used_tokens
        || input.web_search_calls > 0
        || input.web_search_preview_calls > 0
        || input.images_generated > 0
        || input.characters > 0
        || seconds.is_some();
    Cost {
        nano: (!used || !parts.is_empty()).then(|| parts.values().sum()),
        complete,
        parts,
        version: None,
    }
}

/// The price of one generated image for the size and quality of the row.
fn image_price(prices: &Prices, input: &CostInput) -> Option<f64> {
    let size = input.image_size.as_deref()?;
    let quality = input.image_quality.as_deref();
    let exact = prices
        .images
        .iter()
        .find(|p| p.size == size && p.quality.is_some() && p.quality.as_deref() == quality);
    exact
        .or_else(|| prices.images.iter().find(|p| p.size == size && p.quality.is_none()))
        .map(|p| p.usd)
}
