//! Keyed provider connections (OpenAI, Anthropic, OpenRouter).

pub mod catalog;
pub mod models;

use anyhow::Context;
use url::Url;

use crate::outbound;
use crate::state::AppState;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OpenAi,
    Anthropic,
    OpenRouter,
}

impl Kind {
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
        }
    }

    /// Base URL with the API version path; endpoint paths are added to it.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }
}

/// A connection with its decrypted API key.
pub struct Connection {
    pub kind: Kind,
    base_url: Url,
    api_key: String,
}

pub fn key_aad(id: i64) -> String {
    format!("connection:{id}:api_key")
}

impl Connection {
    pub async fn load(state: &AppState, id: i64) -> anyhow::Result<Self> {
        let (kind, base_url, api_key_enc): (String, Option<String>, Vec<u8>) =
            sqlx::query_as("SELECT kind, base_url, api_key_enc FROM connections WHERE id = ?")
                .bind(id)
                .fetch_optional(&state.db)
                .await?
                .context("the connection does not exist")?;
        let kind = Kind::parse(&kind).context("unknown connection kind")?;
        let base_url = Url::parse(base_url.as_deref().unwrap_or(kind.default_base_url()))?;
        let api_key = String::from_utf8(state.secrets.decrypt(&key_aad(id), &api_key_enc)?)?;
        Ok(Self {
            kind,
            base_url,
            api_key,
        })
    }

    pub fn decisions_geo(&self) -> &'static str {
        if self.base_url.scheme() != "https"
            || self.base_url.port_or_known_default() != Some(443)
            || self.base_url.path().trim_end_matches('/') != "/v1"
            || self.base_url.query().is_some()
        {
            return "unknown";
        }
        match self.base_url.host_str() {
            Some("api.openai.com") => "global",
            Some("us.api.openai.com") => "us",
            Some("eu.api.openai.com") => "eu",
            _ => "unknown",
        }
    }

    /// A request to `<base URL>/<path>` with the authentication headers of the provider. The
    /// base URL is checked again here, because a literal IP address does not reach the resolver.
    /// `anthropic_version` is the version header of an Anthropic client, if it sent one.
    pub fn request(
        &self,
        state: &AppState,
        method: reqwest::Method,
        path: &str,
        anthropic_version: Option<&str>,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        outbound::check_url(&self.base_url, state.config.allow_private_upstreams).map_err(anyhow::Error::msg)?;
        let url = format!("{}/{path}", self.base_url.as_str().trim_end_matches('/'));
        let request = state.upstream_http.request(method, url);
        Ok(match self.kind {
            Kind::Anthropic => request
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", anthropic_version.unwrap_or(ANTHROPIC_VERSION)),
            Kind::OpenAi | Kind::OpenRouter => request.bearer_auth(&self.api_key),
        })
    }
}
