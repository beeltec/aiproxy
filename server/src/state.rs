use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRef;
use sqlx::SqlitePool;

use crate::client_ip::TrustedProxies;
use crate::config::Config;
use crate::crypto::PasswordHasher;
use crate::rate_limit::SlidingWindow;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub hasher: PasswordHasher,
    pub login: Arc<LoginLimits>,
}

/// Limits for login and setup attempts.
pub struct LoginLimits {
    /// Attempts per client IP, checked before any password hash runs.
    pub attempts: SlidingWindow,
    /// Failed attempts per client IP and per username. A full window locks the key.
    pub failures: SlidingWindow,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool) -> Self {
        Self {
            config: Arc::new(config),
            db,
            hasher: PasswordHasher::new(),
            login: Arc::new(LoginLimits {
                attempts: SlidingWindow::new(Duration::from_secs(60), 20),
                failures: SlidingWindow::new(Duration::from_secs(15 * 60), 10),
            }),
        }
    }
}

impl FromRef<AppState> for TrustedProxies {
    fn from_ref(state: &AppState) -> Self {
        state.config.trusted_proxies.clone()
    }
}
