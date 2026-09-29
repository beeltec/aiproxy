//! Server-sent events towards the client, with keep-alive pings.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::sync::mpsc;

use super::engine::Msg;

/// A ping after this much silence. Claude Code stops after 300 s without data.
pub(super) const PING_AFTER: Duration = Duration::from_secs(15);

/// Turns engine messages into client SSE bytes. `encode` returns the bytes for one message;
/// `ping` is sent after silence.
pub fn response(
    mut rx: mpsc::Receiver<Msg>,
    mut encode: impl FnMut(Msg) -> Vec<Bytes> + Send + 'static,
    ping: &'static str,
) -> Response {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        loop {
            // The body owns `body_rx`. When it is dropped, dropping `rx` tells the engine to stop.
            let next = tokio::select! {
                () = tx.closed() => return,
                next = tokio::time::timeout(PING_AFTER, rx.recv()) => next,
            };
            let chunks = match next {
                Ok(Some(Msg::Raw(frame))) => vec![frame],
                Ok(Some(msg)) => encode(msg),
                Ok(None) => return,
                Err(_) => vec![Bytes::from_static(ping.as_bytes())],
            };
            for chunk in chunks {
                if tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
        }
    });
    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// One SSE frame. Data with several lines gets one `data:` line each.
pub fn frame(event: Option<&str>, data: &str) -> Bytes {
    let mut out = String::with_capacity(data.len() + 32);
    if let Some(event) = event {
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
    }
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out)
}
