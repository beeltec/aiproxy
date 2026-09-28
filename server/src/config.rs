use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub public_url: Url,
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let public_url = required("AIPROXY_PUBLIC_URL")?;
        let public_url = Url::parse(&public_url).context("AIPROXY_PUBLIC_URL is not a valid URL")?;
        let is_localhost = matches!(public_url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if public_url.scheme() != "https" && !(public_url.scheme() == "http" && is_localhost) {
            bail!("AIPROXY_PUBLIC_URL must use https (http is allowed only for localhost)");
        }
        if public_url.path() != "/" || public_url.query().is_some() {
            bail!("AIPROXY_PUBLIC_URL must be an origin without path or query");
        }

        let bind = bind_from_env()?;
        let data_dir = optional("AIPROXY_DATA_DIR")
            .unwrap_or_else(|| "/data".to_owned())
            .into();

        Ok(Self {
            public_url,
            bind,
            data_dir,
        })
    }

    /// Origin without the trailing slash, as browsers send it in the `Origin` header.
    pub fn public_origin(&self) -> String {
        self.public_url.origin().ascii_serialization()
    }
}

pub fn bind_from_env() -> anyhow::Result<SocketAddr> {
    optional("AIPROXY_BIND")
        .unwrap_or_else(|| "0.0.0.0:8080".to_owned())
        .parse()
        .context("AIPROXY_BIND is not a valid socket address")
}

fn required(name: &str) -> anyhow::Result<String> {
    optional(name).with_context(|| format!("{name} is required"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}
