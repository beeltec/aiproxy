use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use base64::Engine;
use url::Url;

use crate::client_ip::TrustedProxies;

#[derive(Clone, Debug)]
pub struct Config {
    pub public_url: Url,
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub trusted_proxies: TrustedProxies,
    /// Key for secrets at rest (AES-256-GCM).
    pub master_key: [u8; 32],
    /// Connection base URLs may use http and private addresses.
    pub allow_private_upstreams: bool,
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

        let trusted_proxies = TrustedProxies::parse(&optional("AIPROXY_TRUSTED_PROXIES").unwrap_or_default())?;
        let master_key = master_key()?;
        let allow_private_upstreams = match optional("AIPROXY_ALLOW_PRIVATE_UPSTREAMS").as_deref() {
            None | Some("false" | "0") => false,
            Some("true" | "1") => true,
            Some(_) => bail!("AIPROXY_ALLOW_PRIVATE_UPSTREAMS must be true or false"),
        };

        Ok(Self {
            public_url,
            bind,
            data_dir,
            trusted_proxies,
            master_key,
            allow_private_upstreams,
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

/// Reads the master key from `AIPROXY_MASTER_KEY` or from the file in `AIPROXY_MASTER_KEY_FILE`.
fn master_key() -> anyhow::Result<[u8; 32]> {
    let encoded = match (optional("AIPROXY_MASTER_KEY"), optional("AIPROXY_MASTER_KEY_FILE")) {
        (Some(_), Some(_)) => bail!("set only one of AIPROXY_MASTER_KEY and AIPROXY_MASTER_KEY_FILE"),
        (Some(key), None) => key,
        (None, Some(path)) => {
            std::fs::read_to_string(&path).with_context(|| format!("cannot read AIPROXY_MASTER_KEY_FILE {path}"))?
        }
        (None, None) => bail!("AIPROXY_MASTER_KEY is required (create one with: openssl rand -base64 32)"),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("the master key is not valid base64")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("the master key must be 32 bytes (openssl rand -base64 32)"))
}

fn required(name: &str) -> anyhow::Result<String> {
    optional(name).with_context(|| format!("{name} is required"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}
