//! Admin API for the dashboard, below `/admin/api`.

mod account;
mod admins;
mod api_keys;
pub mod auth;
mod factors;
mod instance_settings;
mod passkeys;
mod session;
mod subscriptions;

pub use passkeys::Ceremonies;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::error::ApiError;
use crate::state::AppState;

pub const PREFIX: &str = "/admin/api";

#[derive(OpenApi)]
#[openapi(info(title = "aiproxy admin API", description = "API for the aiproxy dashboard."))]
struct ApiDoc;

fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(auth::router())
        .merge(admins::router())
        .merge(account::router())
        .merge(factors::router())
        .merge(passkeys::router())
        .merge(api_keys::router())
        .merge(subscriptions::router())
        .merge(instance_settings::router())
}

pub fn router(state: AppState) -> Router<AppState> {
    let (router, _) = api().split_for_parts();
    router
        .fallback(|| async { ApiError::not_found("Unknown API path.") })
        .layer(middleware::from_fn_with_state(state, protect))
}

/// The OpenAPI document as JSON, used to generate the dashboard client.
pub fn openapi_json() -> String {
    let (_, api) = api().split_for_parts();
    api.to_pretty_json().expect("OpenAPI document serializes")
}

const MAX_BODY: usize = 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// Blocks cross-site writes, reads the whole body before the handler runs, and disables caching
/// of API answers. With the body read first, the session check in the handler happens after the
/// client sent everything, so a slow body cannot outlive a revoked session.
async fn protect(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && !same_origin(&state, &request) {
        return ApiError::new(StatusCode::FORBIDDEN, "cross_site", "Cross-site request blocked.").into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, MAX_BODY)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The request is too large.")
                .into_response();
        }
        Err(_) => {
            return ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "timeout",
                "The request body came too slowly.",
            )
            .into_response();
        }
    };
    let mut response = next.run(Request::from_parts(parts, Body::from(bytes))).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn same_origin(state: &AppState, request: &Request) -> bool {
    let headers = request.headers();
    match headers.get(header::ORIGIN) {
        Some(origin) => origin.as_bytes() == state.config.public_origin().as_bytes(),
        None => headers.get("sec-fetch-site").is_some_and(|v| v == "same-origin"),
    }
}
