use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
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
    pub webauthn: Arc<Webauthn>,
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
        let rp_id = config
            .public_url
            .host_str()
            .context("AIPROXY_PUBLIC_URL has no host")?
            .to_owned();
        let webauthn = WebauthnBuilder::new(&rp_id, &config.public_url)
            .and_then(|builder| builder.rp_name("aiproxy").build())
            .context("cannot set up passkeys for AIPROXY_PUBLIC_URL")?;
        Ok(Self {
            secrets: SecretBox::new(&config.master_key),
            webauthn: Arc::new(webauthn),
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

impl FromRef<AppState> for TrustedProxies {
    fn from_ref(state: &AppState) -> Self {
        state.config.trusted_proxies.clone()
    }
}
