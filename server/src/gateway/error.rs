use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Which error body the client understands.
#[derive(Clone, Copy, Debug)]
pub enum ErrorFormat {
    OpenAi,
    Anthropic,
}

/// Error answer of the gateway, in the format of the client.
#[derive(Debug)]
pub struct GatewayError {
    pub status: StatusCode,
    /// OpenAI `type` and `code`, for example `invalid_api_key`.
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
    pub format: ErrorFormat,
}

impl GatewayError {
    pub fn new(format: ErrorFormat, status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            format,
        }
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds.max(1));
        self
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let body = match self.format {
            ErrorFormat::OpenAi => json!({
                "error": { "message": self.message, "type": openai_type(self.status), "code": self.code }
            }),
            ErrorFormat::Anthropic => json!({
                "type": "error",
                "error": { "type": anthropic_type(self.status), "message": self.message }
            }),
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

fn openai_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "server_error",
    }
}

fn anthropic_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    }
}
