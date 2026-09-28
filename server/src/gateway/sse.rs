//! Server-sent events towards the client, with keep-alive pings.

use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::sync::mpsc;

use super::auth::Admission;
use super::engine::Msg;

/// A ping after this much silence. Claude Code stops after 300 s without data.
const PING_AFTER: Duration = Duration::from_secs(15);
/// A client that takes no data for this long has stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Turns engine messages into client SSE bytes. `encode` returns the bytes for one message;
/// `ping` is sent after silence. A stalled client loses its concurrency slots.
pub fn response(
    mut rx: mpsc::Receiver<Msg>,
    mut encode: impl FnMut(Msg) -> Vec<Bytes> + Send + 'static,
    ping: &'static str,
    admission: Admission,
) -> Response {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    // Shared, so that a stalled response can drop its queued frames.
    let queue = Arc::new(Mutex::new(Some(body_rx)));
    let stalled = queue.clone();
    tokio::spawn(async move {
        loop {
            // Dropping `rx` when the client is gone tells the engine to stop.
            let next = tokio::select! {
                () = tx.closed() => return,
                next = tokio::time::timeout(PING_AFTER, rx.recv()) => next,
            };
            let chunks = match next {
                Ok(Some(msg)) => encode(msg),
                Ok(None) => break,
                Err(_) => vec![Bytes::from_static(ping.as_bytes())],
            };
            for chunk in chunks {
                match tokio::time::timeout(WRITE_TIMEOUT, tx.send(Ok(chunk))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return,
                    Err(_) => {
                        stalled.lock().expect("queue lock").take();
                        admission.release();
                        return;
                    }
                }
            }
        }
    });
    let stream = futures_util::stream::poll_fn(move |cx| match queue.lock().expect("queue lock").as_mut() {
        Some(rx) => rx.poll_recv(cx),
        None => Poll::Ready(None),
    });
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// One SSE frame.
pub fn frame(event: Option<&str>, data: &str) -> Bytes {
    match event {
        Some(event) => Bytes::from(format!("event: {event}\ndata: {data}\n\n")),
        None => Bytes::from(format!("data: {data}\n\n")),
    }
}
