mod config;
mod web_assets;

use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::http::{HeaderName, HeaderValue, Uri};
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::web_assets::WebAssets;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => serve().await,
        Some("healthcheck") => healthcheck().await,
        Some(other) => anyhow::bail!("unknown command `{other}` (use `serve` or `healthcheck`)"),
    }
}

async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("AIPROXY_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Config::from_env()?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;

    let app = router(WebAssets::new());
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("cannot bind {}", config.bind))?;
    tracing::info!(bind = %config.bind, public_url = %config.public_origin(), "aiproxy started");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("aiproxy stopped");
    Ok(())
}

fn router(assets: WebAssets) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback(move |uri: Uri| {
            let assets = assets.clone();
            async move { assets.serve(&uri) }
        })
        .layer(header("x-content-type-options", "nosniff"))
        .layer(header("referrer-policy", "same-origin"))
        .layer(header("x-frame-options", "DENY"))
        .layer(TraceLayer::new_for_http())
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

/// Container health check. The runtime image has no shell or curl, so the binary checks itself.
async fn healthcheck() -> anyhow::Result<()> {
    let port = config::bind_from_env()?.port();
    let check = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
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
