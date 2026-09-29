//! The price sync: daily on the settings plan and on demand.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use utoipa::ToSchema;

use super::Prices;
use super::sources::{self, LITELLM, MODELS_DEV, OPENROUTER};
use crate::connections::catalog;
use crate::db::now;
use crate::settings;
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(60);
/// Sleep at most this long, so clock changes cannot delay a run for long.
const MAX_SLEEP: Duration = Duration::from_secs(3600);

/// The last sync of one price list.
#[derive(Clone, Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Source {
    pub source: String,
    /// Unix seconds of the last attempt.
    pub fetched_at: Option<i64>,
    /// `ok` or `error`
    pub status: String,
    pub error: Option<String>,
    /// Models with prices in the last good list.
    pub entries: i64,
}

pub async fn sources(db: &SqlitePool) -> Result<Vec<Source>, sqlx::Error> {
    sqlx::query_as("SELECT source, fetched_at, status, error, entries FROM price_sources ORDER BY source")
        .fetch_all(db)
        .await
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let response = http.get(url).timeout(TIMEOUT).send().await?.error_for_status()?;
    Ok(response.json().await?)
}

/// Loads all price lists and saves the changed prices. A list that fails keeps its last good
/// prices. Runs one at a time.
pub async fn sync(state: &AppState) -> Result<(), sqlx::Error> {
    let _running = state.price_sync.lock().await;
    let http = &state.http;
    let (litellm, models_dev, openrouter, embeddings) = tokio::join!(
        fetch(http, LITELLM),
        fetch(http, MODELS_DEV),
        fetch(http, OPENROUTER[0]),
        fetch(http, OPENROUTER[1]),
    );
    // The capability checks use the same lists.
    catalog::store(state, models_dev.as_ref().ok(), litellm.as_ref().ok());

    let openrouter = openrouter.map(|models| {
        // Without the embedding list, the chat models still have prices.
        let mut lists = vec![models];
        match embeddings {
            Ok(embeddings) => lists.push(embeddings),
            Err(err) => tracing::warn!(error = %err, "cannot load the OpenRouter embedding models"),
        }
        lists
    });
    let parsed = [
        ("litellm", litellm.map(|list| sources::parse_litellm(&list))),
        ("models_dev", models_dev.map(|list| sources::parse_models_dev(&list))),
        ("openrouter", openrouter.map(|lists| sources::parse_openrouter(&lists))),
    ];
    for (source, prices) in parsed {
        match prices {
            Ok(prices) => {
                let changed = save(&state.db, source, &prices).await?;
                tracing::info!(source, models = prices.len(), changed, "price list loaded");
                set_status(&state.db, source, None, prices.len()).await?;
            }
            Err(err) => {
                tracing::warn!(source, error = %err, "cannot load a price list");
                set_status(&state.db, source, Some(&format!("{err:#}")), 0).await?;
            }
        }
    }
    state.prices.reload(&state.db).await
}

async fn set_status(db: &SqlitePool, source: &str, error: Option<&str>, entries: usize) -> Result<(), sqlx::Error> {
    // A failed list keeps the count of its last good list.
    sqlx::query(
        "INSERT INTO price_sources (source, fetched_at, status, error, entries) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (source) DO UPDATE SET fetched_at = excluded.fetched_at, status = excluded.status,
             error = excluded.error,
             entries = CASE WHEN excluded.status = 'ok' THEN excluded.entries ELSE price_sources.entries END",
    )
    .bind(source)
    .bind(now())
    .bind(if error.is_some() { "error" } else { "ok" })
    .bind(error)
    .bind(entries as i64)
    .execute(db)
    .await?;
    Ok(())
}

/// Saves a new current version for each model whose prices changed. Returns the count.
async fn save(db: &SqlitePool, source: &str, prices: &BTreeMap<String, Prices>) -> Result<usize, sqlx::Error> {
    let current: Vec<(String, String)> =
        sqlx::query_as("SELECT model_key, prices FROM price_versions WHERE source = ? AND current = 1")
            .bind(source)
            .fetch_all(db)
            .await?;
    let current: HashMap<String, String> = current.into_iter().collect();
    let mut tx = db.begin().await?;
    let mut changed = 0;
    let created = now();
    for (key, prices) in prices {
        let json = serde_json::to_string(prices).expect("prices serialize");
        if current.get(key) == Some(&json) {
            continue;
        }
        sqlx::query("UPDATE price_versions SET current = 0 WHERE source = ? AND model_key = ? AND current = 1")
            .bind(source)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO price_versions (source, model_key, prices, created_at, current) VALUES (?, ?, ?, ?, 1)",
        )
        .bind(source)
        .bind(key)
        .bind(&json)
        .bind(created)
        .execute(&mut *tx)
        .await?;
        changed += 1;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Runs until the process stops. `state.price_schedule_changed` wakes it after a settings
/// change. The first start loads the lists at once.
pub async fn run(state: AppState) {
    let never_synced = sources(&state.db).await.is_ok_and(|sources| sources.is_empty());
    if never_synced && let Err(err) = sync(&state).await {
        tracing::error!(error = %err, "the price sync failed");
    }
    loop {
        let now = Utc::now();
        let next = match settings::load(&state.db).await {
            Ok(settings) if settings.price_sync.enabled => settings::parse_cron(&settings.price_sync.cron)
                .ok()
                .and_then(|cron| settings::next_runs(&cron, settings.tz(), now, 1).into_iter().next()),
            Ok(_) => None,
            Err(err) => {
                tracing::error!(error = %err, "cannot read the price sync plan");
                None
            }
        };
        let sleep = next
            .and_then(|at| (at - now).to_std().ok())
            .unwrap_or(MAX_SLEEP)
            .min(MAX_SLEEP);
        tokio::select! {
            () = tokio::time::sleep(sleep) => {}
            () = state.price_schedule_changed.notified() => continue,
        }
        if next.is_some_and(|at| at <= Utc::now())
            && let Err(err) = sync(&state).await
        {
            tracing::error!(error = %err, "the price sync failed");
        }
    }
}
