//! Server-sent events towards the client, with keep-alive pings.

use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

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
const QUEUE: usize = 64;

struct Queue {
    rx: Option<mpsc::Receiver<Result<Bytes, std::io::Error>>>,
    /// The last time the server read the body.
    polled: Instant,
}

/// Turns engine messages into client SSE bytes. `encode` returns the bytes for one message;
/// `ping` is sent after silence. A stalled client loses its concurrency slots.
pub fn response(
    mut rx: mpsc::Receiver<Msg>,
    mut encode: impl FnMut(Msg) -> Vec<Bytes> + Send + 'static,
    ping: &'static str,
    admission: Admission,
) -> Response {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(QUEUE);
    // The body owns the queue. The task has only a weak link, so a dropped body closes the
    // queue, and a stalled body can lose its queued frames.
    let queue = Arc::new(Mutex::new(Queue {
        rx: Some(body_rx),
        polled: Instant::now(),
    }));
    let link = Arc::downgrade(&queue);
    tokio::spawn(async move {
        let stall = || {
            if let Some(queue) = link.upgrade() {
                queue.lock().expect("queue lock").rx.take();
            }
            admission.release();
        };
        loop {
            // A closed queue tells the engine to stop, because `rx` is dropped.
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
                    Err(_) => return stall(),
                }
            }
        }
        // The server may still be writing the last frames. Until it drops the body, the body
        // must be read again within the write timeout.
        drop(tx);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(queue) = link.upgrade() else {
                return;
            };
            let polled = queue.lock().expect("queue lock").polled;
            drop(queue);
            if polled.elapsed() > WRITE_TIMEOUT {
                return stall();
            }
        }
    });
    let stream = futures_util::stream::poll_fn(move |cx| {
        let mut queue = queue.lock().expect("queue lock");
        queue.polled = Instant::now();
        match queue.rx.as_mut() {
            Some(rx) => rx.poll_recv(cx),
            None => Poll::Ready(None),
        }
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
