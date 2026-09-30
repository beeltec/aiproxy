//! Requests to the ChatGPT Codex backend. The headers follow the Codex CLI.

use reqwest::RequestBuilder;
use sqlx::SqlitePool;

use super::accounts::Credentials;
use super::oauth::ORIGINATOR;

pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Adds the headers that every backend request needs.
pub fn with_headers(
    request: RequestBuilder,
    credentials: &Credentials,
    installation_id: &str,
    client_version: &str,
) -> RequestBuilder {
    request
        .bearer_auth(&credentials.access_token)
        .header("ChatGPT-Account-ID", &credentials.chatgpt_account_id)
        .header("originator", ORIGINATOR)
        .header("User-Agent", format!("{ORIGINATOR}/{client_version} (aiproxy)"))
        .header("x-codex-installation-id", installation_id)
}

/// A random id for this installation, created once and stored in the settings table.
pub async fn installation_id(db: &SqlitePool) -> Result<String, sqlx::Error> {
    let new_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('installation_id', ?) ON CONFLICT (key) DO NOTHING")
        .bind(&new_id)
        .execute(db)
        .await?;
    sqlx::query_scalar("SELECT value FROM settings WHERE key = 'installation_id'")
        .fetch_one(db)
        .await
}
