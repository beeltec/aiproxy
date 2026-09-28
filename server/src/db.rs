use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub async fn open(data_dir: &Path) -> anyhow::Result<SqlitePool> {
    let path = data_dir.join("aiproxy.db");
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .with_context(|| format!("cannot open database {}", path.display()))?;
    sqlx::migrate!().run(&pool).await.context("database migration failed")?;
    Ok(pool)
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
