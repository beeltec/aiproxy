//! Usage records. Handlers send rows to one writer task through a bounded queue.

use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};

/// Token counts as billing categories that do not overlap.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub input_text: i64,
    pub input_text_cached: i64,
    pub input_audio: i64,
    pub input_audio_cached: i64,
    pub cache_write_5m: i64,
    pub output_text: i64,
    pub output_reasoning: i64,
    pub output_audio: i64,
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
        }
    }

    pub fn input(&self) -> i64 {
        self.input_text + self.input_text_cached + self.input_audio + self.input_audio_cached + self.cache_write_5m
    }

    pub fn output(&self) -> i64 {
        self.output_text + self.output_reasoning + self.output_audio
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub request_id: String,
    pub time: i64,
    pub api_key_id: i64,
    pub route: &'static str,
    pub client_format: &'static str,
    pub upstream: &'static str,
    pub chatgpt_account_id: Option<i64>,
    pub requested_model: String,
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
    pub web_search_calls: i64,
    pub failover_attempts: i64,
}

enum Job {
    Row(Box<Row>),
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
    pub fn start(db: SqlitePool) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Job>(QUEUE);
        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                match job {
                    Job::Row(row) => {
                        if let Err(err) = insert(&db, &row).await {
                            tracing::error!(error = %err, request = %row.request_id, "cannot save a usage row");
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
        if self.sender.send(Job::Row(Box::new(row))).await.is_err() {
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

async fn insert(db: &SqlitePool, row: &Row) -> Result<(), sqlx::Error> {
    let t = &row.tokens;
    sqlx::query(
        "INSERT INTO usage (request_id, time, api_key_id, route, client_format, upstream, chatgpt_account_id,
             requested_model, resolved_model, effort, service_tier_requested, service_tier_reported, streamed,
             status_code, error_kind, latency_ms, first_token_ms, usage_status, usage_exact,
             input_text, input_text_cached, input_audio, input_audio_cached, cache_write_5m,
             output_text, output_reasoning, output_audio, web_search_calls, failover_attempts)
         VALUES (?, ?, (SELECT id FROM api_keys WHERE id = ?), ?, ?, ?, (SELECT id FROM chatgpt_accounts WHERE id = ?),
             ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.request_id)
    .bind(row.time)
    .bind(row.api_key_id)
    .bind(row.route)
    .bind(row.client_format)
    .bind(row.upstream)
    .bind(row.chatgpt_account_id)
    .bind(&row.requested_model)
    .bind(&row.resolved_model)
    .bind(&row.effort)
    .bind(&row.service_tier_requested)
    .bind(&row.service_tier_reported)
    .bind(row.streamed)
    .bind(i64::from(row.status_code))
    .bind(&row.error_kind)
    .bind(row.latency_ms)
    .bind(row.first_token_ms)
    .bind(row.usage_status)
    .bind(row.usage_status == "reported")
    .bind(t.input_text)
    .bind(t.input_text_cached)
    .bind(t.input_audio)
    .bind(t.input_audio_cached)
    .bind(t.cache_write_5m)
    .bind(t.output_text)
    .bind(t.output_reasoning)
    .bind(t.output_audio)
    .bind(row.web_search_calls)
    .bind(row.failover_attempts)
    .execute(db)
    .await?;
    Ok(())
}
