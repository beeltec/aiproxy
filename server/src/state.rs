use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use axum::extract::FromRef;
use sqlx::SqlitePool;
use webauthn_rs::{Webauthn, WebauthnBuilder};

use tokio::sync::Notify;

use crate::admin::Ceremonies;
use crate::chatgpt::link::LinkFlows;
use crate::chatgpt::refresh::Refresher;
use crate::client_ip::TrustedProxies;
use crate::config::Config;
use crate::connections::catalog::CatalogCache;
use crate::crypto::{PasswordHasher, SecretBox};
use crate::gateway::{ItemCache, KeyLimits, RejectedCounter, ThinkingCache};
use crate::prices::PriceCache;
use crate::prices::recompute::Recompute;
use crate::rate_limit::SlidingWindow;
use crate::usage::UsageWriter;

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
    pub key_limits: Arc<KeyLimits>,
    pub rejected: Arc<RejectedCounter>,
    /// HTTP client for fixed upstreams (OpenAI, ChatGPT). It follows no redirects.
    pub http: reqwest::Client,
    /// HTTP client for connection base URLs, with the outbound address policy.
    pub upstream_http: reqwest::Client,
    pub catalog: Arc<CatalogCache>,
    pub thinking_cache: Arc<ThinkingCache>,
    pub item_cache: Arc<ItemCache>,
    pub refresher: Arc<Refresher>,
    pub link_flows: Arc<LinkFlows>,
    /// Wakes the refresh scheduler after changes to the plans.
    pub schedule_changed: Arc<Notify>,
    pub usage: UsageWriter,
    /// The current prices; the usage writer computes the cost of each row with them.
    pub prices: PriceCache,
    /// Only one price sync runs at a time.
    pub price_sync: Arc<tokio::sync::Mutex<()>>,
    /// Wakes the price sync scheduler after a settings change.
    pub price_schedule_changed: Arc<Notify>,
    pub recompute: Arc<Recompute>,
    /// Running gateway requests, so that shutdown can wait for their usage rows.
    pub gateway_tasks: TaskTracker,
    /// Cancelled at shutdown: running gateway requests stop and record their usage.
    pub stopping: CancellationToken,
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
        let prices = PriceCache::default();
        Ok(Self {
            secrets: SecretBox::new(&config.master_key),
            webauthn: webauthn.map(Arc::new),
            ceremonies: Arc::default(),
            key_limits: Arc::default(),
            rejected: Arc::default(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .build()?,
            upstream_http: crate::outbound::client(config.allow_private_upstreams)?,
            catalog: Arc::default(),
            thinking_cache: Arc::default(),
            item_cache: Arc::default(),
            refresher: Arc::default(),
            link_flows: Arc::default(),
            schedule_changed: Arc::default(),
            usage: UsageWriter::start(db.clone(), prices.clone()),
            prices,
            price_sync: Arc::default(),
            price_schedule_changed: Arc::default(),
            recompute: Arc::default(),
            gateway_tasks: TaskTracker::new(),
            stopping: CancellationToken::new(),
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
