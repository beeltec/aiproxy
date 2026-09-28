use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRef;
use sqlx::SqlitePool;
use webauthn_rs::{Webauthn, WebauthnBuilder};

use crate::admin::Ceremonies;
use crate::client_ip::TrustedProxies;
use crate::config::Config;
use crate::crypto::{PasswordHasher, SecretBox};
use crate::rate_limit::SlidingWindow;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub hasher: PasswordHasher,
    pub login: Arc<LoginLimits>,
    pub secrets: SecretBox,
    /// `None` when the public URL has no host name (passkeys need one).
    pub webauthn: Option<Arc<Webauthn>>,
    pub ceremonies: Arc<Ceremonies>,
}

/// Limits for login and setup attempts.
pub struct LoginLimits {
    /// Attempts per client IP, checked before any password hash runs.
    pub attempts: SlidingWindow,
    /// Failed attempts per client IP and per username. A full window locks the key.
    pub failures: SlidingWindow,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool) -> anyhow::Result<Self> {
        let webauthn = passkeys(&config);
        Ok(Self {
            secrets: SecretBox::new(&config.master_key),
            webauthn: webauthn.map(Arc::new),
            ceremonies: Arc::default(),
            config: Arc::new(config),
            db,
            hasher: PasswordHasher::new(),
            login: Arc::new(LoginLimits {
                attempts: SlidingWindow::new(Duration::from_secs(60), 20),
                failures: SlidingWindow::new(Duration::from_secs(15 * 60), 10),
            }),
        })
    }
}

fn passkeys(config: &Config) -> Option<Webauthn> {
    let url = &config.public_url;
    let webauthn = url
        .domain()
        .ok_or_else(|| anyhow::anyhow!("the host is not a domain name"))
        .and_then(|rp_id| {
            WebauthnBuilder::new(rp_id, url)
                .and_then(|builder| builder.rp_name("aiproxy").build())
                .map_err(anyhow::Error::from)
        });
    match webauthn {
        Ok(webauthn) => Some(webauthn),
        Err(err) => {
            tracing::warn!(public_url = %url, error = %err, "passkeys are off: they need a host name in AIPROXY_PUBLIC_URL");
            None
        }
    }
}

impl FromRef<AppState> for TrustedProxies {
    fn from_ref(state: &AppState) -> Self {
        state.config.trusted_proxies.clone()
    }
}
