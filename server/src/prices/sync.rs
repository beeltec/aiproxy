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

/// An owned copy of a download error that is still in use.
fn copy(err: &anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("{err:#}")
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
    // A list is used only when it has the expected content.
    let litellm_prices = litellm.as_ref().map_err(copy).and_then(sources::parse_litellm);
    let models_dev_prices = models_dev.as_ref().map_err(copy).and_then(sources::parse_models_dev);

    // The capability checks use the same lists: the stored model data of the connections is
    // loaded again with them (admin changes to capabilities stay).
    catalog::store(
        state,
        models_dev.as_ref().ok().filter(|_| models_dev_prices.is_ok()),
        litellm.as_ref().ok().filter(|_| litellm_prices.is_ok()),
    );
    let connections: Vec<i64> = sqlx::query_scalar("SELECT id FROM connections")
        .fetch_all(&state.db)
        .await?;
    for connection in connections {
        if let Err(err) = crate::connections::models::sync(state, connection).await {
            tracing::warn!(connection, error = %err, "cannot load the connection models");
        }
    }

    // Without the embedding list, the chat models still get their prices, but no price ends
    // and the status shows the error.
    let embeddings = embeddings.and_then(|list| match list["data"].is_array() {
        true => Ok(list),
        false => Err(anyhow::anyhow!("The list has not the expected content.")),
    });
    let embeddings_error = embeddings
        .as_ref()
        .err()
        .map(|err| format!("The embedding model list failed: {err:#}"));
    let openrouter = openrouter.map(|models| {
        let mut lists = vec![models];
        lists.extend(embeddings.ok());
        lists
    });
    let parsed = [
        ("litellm", litellm_prices, None),
        ("models_dev", models_dev_prices, None),
        (
            "openrouter",
            openrouter.and_then(|lists| sources::parse_openrouter(&lists)),
            embeddings_error,
        ),
    ];
    for (source, prices, partial_error) in parsed {
        match prices {
            Ok(prices) => {
                let changed = save(&state.db, source, &prices, partial_error.is_none()).await?;
                tracing::info!(source, models = prices.len(), changed, "price list loaded");
                if let Some(error) = &partial_error {
                    tracing::warn!(source, error, "a part of a price list failed");
                }
                set_status(&state.db, source, partial_error.as_deref(), Some(prices.len())).await?;
            }
            Err(err) => {
                tracing::warn!(source, error = %err, "cannot load a price list");
                set_status(&state.db, source, Some(&format!("{err:#}")), None).await?;
            }
        }
    }
    state.prices.reload(&state.db).await
}

/// `entries` is the count of saved prices; a list that failed (`None`) keeps its last count.
async fn set_status(
    db: &SqlitePool,
    source: &str,
    error: Option<&str>,
    entries: Option<usize>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO price_sources (source, fetched_at, status, error, entries)
         VALUES (?1, ?2, ?3, ?4, COALESCE(?5, 0))
         ON CONFLICT (source) DO UPDATE SET fetched_at = excluded.fetched_at, status = excluded.status,
             error = excluded.error, entries = COALESCE(?5, price_sources.entries)",
    )
    .bind(source)
    .bind(now())
    .bind(if error.is_some() { "error" } else { "ok" })
    .bind(error)
    .bind(entries.map(|n| n as i64))
    .execute(db)
    .await?;
    Ok(())
}

/// Saves a new current version for each model whose prices changed. When the list is
/// `complete`, the current version of models that it no longer prices ends. Returns the count
/// of changes.
async fn save(
    db: &SqlitePool,
    source: &str,
    prices: &BTreeMap<String, Prices>,
    complete: bool,
) -> Result<usize, sqlx::Error> {
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
    for key in current.keys().filter(|key| complete && !prices.contains_key(*key)) {
        sqlx::query("UPDATE price_versions SET current = 0 WHERE source = ? AND model_key = ? AND current = 1")
            .bind(source)
            .bind(key)
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
