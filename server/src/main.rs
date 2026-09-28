mod admin;
mod chatgpt;
mod client_ip;
mod config;
mod crypto;
mod db;
mod error;
mod gateway;
mod rate_limit;
mod settings;
mod state;
mod web_assets;

use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Uri};
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;
use crate::web_assets::WebAssets;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The database holds password hashes and secrets: new files are for the owner only.
    #[cfg(unix)]
    // SAFETY: umask only changes the file mode mask of this process.
    unsafe {
        libc::umask(0o077);
    }
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => serve().await,
        Some("healthcheck") => healthcheck().await,
        Some("setup-token") => setup_token().await,
        Some("openapi") => {
            println!("{}", admin::openapi_json());
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command `{other}` (use serve, healthcheck, setup-token or openapi)"),
    }
}

async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("AIPROXY_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Config::from_env()?;

    let db = db::open(&config.data_dir).await?;
    let bind = config.bind;
    let public_origin = config.public_origin();
    let state = AppState::new(config, db)?;
    let rejected = state.rejected.clone();
    let rejected_db = state.db.clone();
    tokio::spawn(async move { rejected.run(rejected_db).await });
    tokio::spawn(chatgpt::scheduler::run(state.clone()));
    let app = router(state.clone(), WebAssets::new());
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    tracing::info!(%bind, public_url = %public_origin, "aiproxy started");

    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel();
    let app = app.into_make_service_with_connect_info::<SocketAddr>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal().await;
        let _ = signalled_tx.send(());
    });
    let deadline = async {
        if signalled_rx.await.is_ok() {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        result = server.into_future() => result?,
        () = deadline => tracing::warn!("open connections did not close in time, stopping now"),
    }
    // A refresh that is running must save its rotated token before the process ends.
    if tokio::time::timeout(SHUTDOWN_GRACE, state.refresher.drain())
        .await
        .is_err()
    {
        tracing::warn!("token refreshes did not end in time");
    }
    state.rejected.flush(&state.db).await;
    tracing::info!("aiproxy stopped");
    Ok(())
}

fn router(state: AppState, assets: WebAssets) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest(
            admin::PREFIX,
            admin::router(state.clone()).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .nest(gateway::PREFIX, gateway::router(state.clone()))
        .fallback(move |uri: Uri| {
            let assets = assets.clone();
            async move { assets.serve(&uri) }
        })
        .layer(header("x-content-type-options", "nosniff"))
        .layer(header("referrer-policy", "same-origin"))
        .layer(header("x-frame-options", "DENY"))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn header(name: &'static str, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(HeaderName::from_static(name), HeaderValue::from_static(value))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("cannot listen for Ctrl+C");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("cannot listen for SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// Creates the one-time token for the first admin and prints it to stdout (not to the server log).
async fn setup_token() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let db = db::open(&config.data_dir).await?;
    let token = admin::auth::create_setup_token(&db).await?;
    println!("Setup token (valid for 1 hour): {token}");
    println!("Open {}/setup and enter the token.", config.public_origin());
    Ok(())
}

/// Container health check. The runtime image has no shell or curl, so the binary checks itself.
async fn healthcheck() -> anyhow::Result<()> {
    let bind = config::bind_from_env()?;
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let check = async {
        let mut stream = TcpStream::connect(SocketAddr::new(ip, bind.port())).await?;
        stream
            .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        anyhow::Ok(response.starts_with(b"HTTP/1.0 200") || response.starts_with(b"HTTP/1.1 200"))
    };
    match tokio::time::timeout(Duration::from_secs(5), check).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => anyhow::bail!("health endpoint did not return 200"),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("health check timed out"),
    }
}
