//! Usage records. Handlers send rows to one writer task through a bounded queue.

use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};

use crate::prices::{Cost, CostInput, PriceCache};
use tokio::sync::{mpsc, oneshot};

/// Token counts as billing categories that do not overlap.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub input_text: i64,
    pub input_text_cached: i64,
    pub input_audio: i64,
    pub input_audio_cached: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub input_image: i64,
    pub input_image_cached: i64,
    pub output_text: i64,
    pub output_reasoning: i64,
    pub output_audio: i64,
    pub output_image: i64,
    /// A split between categories was guessed, so the cost cannot be complete.
    pub inexact: bool,
}

impl Tokens {
    /// Splits OpenAI usage (Responses or Chat) into categories. OpenAI totals include cached,
    /// cache-write and audio tokens, and the output total includes reasoning and audio.
    pub fn from_openai(usage: &Value) -> Self {
        let n = |v: &Value| v.as_i64().unwrap_or(0).max(0);
        let input_total = n(&usage["input_tokens"]).max(n(&usage["prompt_tokens"]));
        let output_total = n(&usage["output_tokens"]).max(n(&usage["completion_tokens"]));
        let input_details = if usage["input_tokens_details"].is_object() {
            &usage["input_tokens_details"]
        } else {
            &usage["prompt_tokens_details"]
        };
        let output_details = if usage["output_tokens_details"].is_object() {
            &usage["output_tokens_details"]
        } else {
            &usage["completion_tokens_details"]
        };
        let cached = n(&input_details["cached_tokens"]);
        let cache_write = n(&input_details["cache_write_tokens"]);
        let audio = n(&input_details["audio_tokens"]);
        let reasoning = n(&output_details["reasoning_tokens"]);
        let output_audio = n(&output_details["audio_tokens"]);
        Self {
            input_text: (input_total - cached - cache_write - audio).max(0),
            input_text_cached: cached,
            input_audio: audio,
            input_audio_cached: 0,
            cache_write_5m: cache_write,
            output_text: (output_total - reasoning - output_audio).max(0),
            output_reasoning: reasoning,
            output_audio,
            ..Self::default()
        }
    }

    /// Splits Anthropic usage. `input_tokens` has no cached or cache-write tokens. When only the
    /// cache-write total is given, `ttl` is the cache time of the request: `Some("1h")` or
    /// `Some("5m")` when all `cache_control` entries agree, `None` when they are mixed.
    pub fn from_anthropic(usage: &Value, ttl: Option<&str>) -> Self {
        let n = |v: &Value| v.as_i64().unwrap_or(0).max(0);
        let creation = &usage["cache_creation"];
        let (write_5m, write_1h, inexact) = if creation.is_object() {
            (
                n(&creation["ephemeral_5m_input_tokens"]),
                n(&creation["ephemeral_1h_input_tokens"]),
                false,
            )
        } else {
            let total = n(&usage["cache_creation_input_tokens"]);
            match ttl {
                Some("1h") => (0, total, false),
                Some(_) => (total, 0, false),
                None => (total, 0, total > 0),
            }
        };
        let output = n(&usage["output_tokens"]);
        let thinking = n(&usage["output_tokens_details"]["thinking_tokens"]).min(output);
        Self {
            input_text: n(&usage["input_tokens"]),
            input_text_cached: n(&usage["cache_read_input_tokens"]),
            cache_write_5m: write_5m,
            cache_write_1h: write_1h,
            output_text: output - thinking,
            output_reasoning: thinking,
            inexact,
            ..Self::default()
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.input_text += other.input_text;
        self.input_text_cached += other.input_text_cached;
        self.input_audio += other.input_audio;
        self.input_audio_cached += other.input_audio_cached;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.input_image += other.input_image;
        self.input_image_cached += other.input_image_cached;
        self.output_text += other.output_text;
        self.output_reasoning += other.output_reasoning;
        self.output_audio += other.output_audio;
        self.output_image += other.output_image;
        self.inexact |= other.inexact;
    }

    /// Splits the usage of an image model (Images API, image-generation tool). Without cache
    /// details the cached count is 0 when `cache_known` (the Images API has no cache price), else
    /// unknown, and then the split is not exact.
    pub fn from_image(usage: &Value, cache_known: bool) -> Self {
        let n = |v: &Value| v.as_i64().unwrap_or(0).max(0);
        let input = &usage["input_tokens_details"];
        let output = &usage["output_tokens_details"];
        let text = n(&input["text_tokens"]);
        let image = n(&input["image_tokens"]);
        // A cached count without a split goes to text first.
        let cached = n(&input["cached_tokens"]);
        let text_cached = cached.min(text);
        let image_cached = (cached - text_cached).min(image);
        let (output_image, output_text) = if output.is_object() {
            (n(&output["image_tokens"]), n(&output["text_tokens"]))
        } else {
            (n(&usage["output_tokens"]), 0)
        };
        Self {
            input_text: text - text_cached,
            input_text_cached: text_cached,
            input_image: image - image_cached,
            input_image_cached: image_cached,
            output_text,
            output_image,
            inexact: (!cache_known && input["cached_tokens"].is_null()) || (cached > 0 && text > 0 && image > 0),
            ..Self::default()
        }
    }

    /// The usage in the Responses shape, for clients.
    pub fn to_responses_usage(&self) -> Value {
        let input = self.input();
        let output = self.output();
        serde_json::json!({
            "input_tokens": input,
            "input_tokens_details": { "cached_tokens": self.input_text_cached + self.input_audio_cached + self.input_image_cached },
            "output_tokens": output,
            "output_tokens_details": { "reasoning_tokens": self.output_reasoning },
            "total_tokens": input + output,
        })
    }

    pub fn input(&self) -> i64 {
        self.input_text
            + self.input_text_cached
            + self.input_audio
            + self.input_audio_cached
            + self.cache_write_5m
            + self.cache_write_1h
            + self.input_image
            + self.input_image_cached
    }

    pub fn output(&self) -> i64 {
        self.output_text + self.output_reasoning + self.output_audio + self.output_image
    }
}

/// One sampling step of an answer. Each step has its own usage row, so that the price of long
/// requests applies per step.
#[derive(Clone, Debug, Default)]
pub struct Step {
    pub tokens: Tokens,
    /// The facts of the answer that the step belongs to (speed, geography).
    pub extras: Extras,
    /// The final usage of the step is missing; its tokens are estimated.
    pub estimated: bool,
}

impl Step {
    /// The steps of an Anthropic answer. With `iterations` (compaction), each step has its own
    /// usage, and the top-level usage leaves some steps out. Without them, the answer is one
    /// step.
    pub fn anthropic(usage: &Value, ttl: Option<&str>) -> Vec<Self> {
        let extras = Extras::from_usage(usage);
        let step = |tokens| Self {
            tokens,
            extras: extras.clone(),
            estimated: false,
        };
        match usage["iterations"].as_array().filter(|steps| !steps.is_empty()) {
            Some(steps) => steps.iter().map(|s| step(Tokens::from_anthropic(s, ttl))).collect(),
            None => vec![step(Tokens::from_anthropic(usage, ttl))],
        }
    }

    pub fn total(steps: &[Self]) -> Tokens {
        let mut total = Tokens::default();
        steps.iter().for_each(|step| total.add(&step.tokens));
        total
    }
}

/// Usage facts besides the token categories.
#[derive(Clone, Debug, Default)]
pub struct Extras {
    /// Anthropic `speed` (`fast` or `standard`).
    pub speed: Option<String>,
    /// Anthropic `inference_geo`.
    pub inference_geo: Option<String>,
    /// The cost that the provider reported (OpenRouter `cost`, in USD), in nano-USD.
    pub reported_cost_nano: Option<i64>,
    pub accepted_prediction_tokens: i64,
    pub rejected_prediction_tokens: i64,
}

impl Extras {
    /// Reads the facts of an OpenAI, OpenRouter or Anthropic usage object.
    pub fn from_usage(usage: &Value) -> Self {
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let details = &usage["completion_tokens_details"];
        Self {
            speed: text(&usage["speed"]),
            inference_geo: text(&usage["inference_geo"]),
            reported_cost_nano: usage["cost"].as_f64().map(|usd| (usd * 1e9).round() as i64),
            accepted_prediction_tokens: details["accepted_prediction_tokens"].as_i64().unwrap_or(0).max(0),
            rejected_prediction_tokens: details["rejected_prediction_tokens"].as_i64().unwrap_or(0).max(0),
        }
    }
}

/// Quantities of media requests and of the image-generation tool.
#[derive(Clone, Debug, Default)]
pub struct Media {
    pub images_generated: i64,
    pub image_size: Option<String>,
    pub image_quality: Option<String>,
    pub input_images: i64,
    pub characters: i64,
    pub seconds: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub request_id: String,
    /// `model`; `iteration` for each further sampling step of an Anthropic answer; `image_tool`
    /// for the image-generation tool.
    pub component: &'static str,
    pub time: i64,
    pub api_key_id: i64,
    pub route: &'static str,
    pub client_format: &'static str,
    pub upstream: &'static str,
    pub chatgpt_account_id: Option<i64>,
    pub connection_id: Option<i64>,
    pub requested_model: String,
    /// The alias name when the client used one.
    pub alias: Option<String>,
    pub resolved_model: Option<String>,
    pub effort: Option<String>,
    pub service_tier_requested: Option<String>,
    pub service_tier_reported: Option<String>,
    pub streamed: bool,
    pub status_code: u16,
    pub error_kind: Option<String>,
    pub latency_ms: i64,
    pub first_token_ms: Option<i64>,
    /// `reported`, `estimated` or `none`.
    pub usage_status: &'static str,
    pub tokens: Tokens,
    /// Calls of the `web_search` tool (also Anthropic web search).
    pub web_search_calls: i64,
    pub web_search_preview_calls: i64,
    pub failover_attempts: i64,
    pub media: Media,
    pub extras: Extras,
}

enum Job {
    /// The rows of one request (its billing components).
    Rows(Vec<Row>),
    /// Answers when all rows queued before it are saved.
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct UsageWriter {
    sender: mpsc::Sender<Job>,
}

const QUEUE: usize = 1000;

impl UsageWriter {
    /// Starts the writer task.
    pub fn start(db: SqlitePool, prices: PriceCache) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Job>(QUEUE);
        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                match job {
                    Job::Rows(rows) => {
                        if let Err(err) = insert_all(&db, &prices, &rows).await {
                            let request = rows.first().map(|row| row.request_id.as_str()).unwrap_or_default();
                            tracing::error!(error = %err, request, "cannot save the usage rows");
                        }
                    }
                    Job::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self { sender }
    }

    /// Queues a row. When the queue is full, this waits (no row is lost).
    pub async fn record(&self, row: Row) {
        self.record_all(vec![row]).await;
    }

    /// Queues the rows of one request. They are saved together or not at all.
    pub async fn record_all(&self, rows: Vec<Row>) {
        if self.sender.send(Job::Rows(rows)).await.is_err() {
            tracing::error!("the usage writer stopped");
        }
    }

    /// Waits until the rows queued before this call are saved.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.sender.send(Job::Flush(done)).await.is_ok() {
            let _ = wait.await;
        }
    }
}

async fn insert_all(db: &SqlitePool, prices: &PriceCache, rows: &[Row]) -> Result<(), sqlx::Error> {
    let book = prices.get();
    let mut tx = db.begin().await?;
    for row in rows {
        let cost = book.cost(&cost_input(row));
        insert(&mut tx, row, &cost).await?;
    }
    tx.commit().await
}

fn cost_input(row: &Row) -> CostInput {
    let mut input = CostInput {
        component: row.component.to_owned(),
        upstream: row.upstream.to_owned(),
        resolved_model: row.resolved_model.clone(),
        usage_status: row.usage_status.to_owned(),
        usage_exact: usage_exact(row),
        service_tier_reported: row.service_tier_reported.clone(),
        speed: row.extras.speed.clone(),
        inference_geo: row.extras.inference_geo.clone(),
        web_search_calls: row.web_search_calls,
        web_search_preview_calls: row.web_search_preview_calls,
        images_generated: row.media.images_generated,
        image_size: row.media.image_size.clone(),
        image_quality: row.media.image_quality.clone(),
        characters: row.media.characters,
        seconds: row.media.seconds,
        ..CostInput::default()
    };
    input.set_tokens(&row.tokens);
    input
}

fn usage_exact(row: &Row) -> bool {
    row.usage_status == "reported" && !row.tokens.inexact
}

async fn insert(db: &mut SqliteConnection, row: &Row, cost: &Cost) -> Result<(), sqlx::Error> {
    let t = &row.tokens;
    // Rows of deleted keys, accounts or connections keep NULL, so the insert does not fail on
    // the foreign key.
    sqlx::query(
        "INSERT INTO usage (request_id, component, time, api_key_id, route, client_format, upstream, chatgpt_account_id,
             connection_id, requested_model, resolved_model, alias, effort, service_tier_requested,
             service_tier_reported, streamed, status_code, error_kind, latency_ms, first_token_ms, usage_status,
             usage_exact, input_text, input_text_cached, input_audio, input_audio_cached, cache_write_5m,
             cache_write_1h, input_image, input_image_cached, output_text, output_reasoning, output_audio,
             output_image, web_search_calls, failover_attempts, images_generated, image_size, image_quality,
             input_images, characters, seconds, web_search_preview_calls, speed, inference_geo,
             accepted_prediction_tokens, rejected_prediction_tokens, reported_cost_nano, cost_nano,
             cost_complete, cost_parts, price_version_id)
         VALUES (?, ?, ?, (SELECT id FROM api_keys WHERE id = ?), ?, ?, ?, (SELECT id FROM chatgpt_accounts WHERE id = ?),
             (SELECT id FROM connections WHERE id = ?), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
             ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.request_id)
    .bind(row.component)
    .bind(row.time)
    .bind(row.api_key_id)
    .bind(row.route)
    .bind(row.client_format)
    .bind(row.upstream)
    .bind(row.chatgpt_account_id)
    .bind(row.connection_id)
    .bind(&row.requested_model)
    .bind(&row.resolved_model)
    .bind(&row.alias)
    .bind(&row.effort)
    .bind(&row.service_tier_requested)
    .bind(&row.service_tier_reported)
    .bind(row.streamed)
    .bind(i64::from(row.status_code))
    .bind(&row.error_kind)
    .bind(row.latency_ms)
    .bind(row.first_token_ms)
    .bind(row.usage_status)
    .bind(usage_exact(row))
    .bind(t.input_text)
    .bind(t.input_text_cached)
    .bind(t.input_audio)
    .bind(t.input_audio_cached)
    .bind(t.cache_write_5m)
    .bind(t.cache_write_1h)
    .bind(t.input_image)
    .bind(t.input_image_cached)
    .bind(t.output_text)
    .bind(t.output_reasoning)
    .bind(t.output_audio)
    .bind(t.output_image)
    .bind(row.web_search_calls)
    .bind(row.failover_attempts)
    .bind(row.media.images_generated)
    .bind(&row.media.image_size)
    .bind(&row.media.image_quality)
    .bind(row.media.input_images)
    .bind(row.media.characters)
    .bind(row.media.seconds)
    .bind(row.web_search_preview_calls)
    .bind(&row.extras.speed)
    .bind(&row.extras.inference_geo)
    .bind(row.extras.accepted_prediction_tokens)
    .bind(row.extras.rejected_prediction_tokens)
    .bind(row.extras.reported_cost_nano)
    .bind(cost.nano)
    .bind(cost.complete)
    .bind(cost.parts_json())
    .bind(cost.version)
    .execute(db)
    .await?;
    Ok(())
}
