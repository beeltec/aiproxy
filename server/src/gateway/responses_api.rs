//! `POST /v1/responses` for Responses clients. The request goes to the backend almost as it is.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::auth::ApiKey;
use super::engine::{self, Failure, Msg};
use super::error::{ErrorFormat, GatewayError};
use super::{request, sse};
use crate::state::AppState;

pub async fn create(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKey>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let hint = headers
        .get("session_id")
        .or_else(|| headers.get("session-id"))
        .and_then(|v| v.to_str().ok());
    let prepared = match request::prepare(&state, &key, body, "responses", "responses", hint).await {
        Ok(prepared) => prepared,
        Err(failure) => return error(failure),
    };
    let stream = prepared.job.stream;
    let rx = match engine::start(&state, prepared.job).await {
        Ok(rx) => rx,
        Err(failure) => return error(failure),
    };
    if stream {
        return sse::response(rx, encode_event, ": ping\n\n");
    }
    collect(rx).await
}

fn encode_event(msg: Msg) -> Vec<bytes::Bytes> {
    match msg {
        Msg::Event(event) => vec![sse::frame(Some(&event.kind), &event.data.to_string())],
        Msg::Done(_) => Vec::new(),
        Msg::Failed(failure) => {
            let data = json!({ "type": "error", "code": failure.code, "message": failure.message });
            vec![sse::frame(Some("error"), &data.to_string())]
        }
    }
}

/// Non-stream clients get the final response object once the stream ended.
async fn collect(mut rx: tokio::sync::mpsc::Receiver<Msg>) -> Response {
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Done(response) => return Json(response).into_response(),
            Msg::Failed(failure) => return error(failure),
            Msg::Event(_) => {}
        }
    }
    error(Failure::new(
        axum::http::StatusCode::BAD_GATEWAY,
        "upstream_closed",
        "The request ended without an answer.",
    ))
}

pub fn error(failure: Failure) -> Response {
    let mut error = GatewayError::new(ErrorFormat::OpenAi, failure.status, failure.code, failure.message);
    if let Some(seconds) = failure.retry_after {
        error = error.with_retry_after(seconds);
    }
    error.into_response()
}
