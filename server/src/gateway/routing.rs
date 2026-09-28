//! Resolves the model name of a request to an upstream model.

use serde_json::Value;
use sqlx::SqlitePool;

/// Where a request goes.
#[derive(Clone, Debug)]
pub struct Route {
    /// The name the client sent.
    pub requested: String,
    /// `chatgpt/<model>`.
    pub qualified: String,
    /// The model id at the upstream.
    pub upstream_model: String,
    /// Row id in the `models` table.
    pub model_id: i64,
    pub capabilities: Value,
}

#[derive(Debug)]
pub enum RouteError {
    NotFound(String),
    Ambiguous(String, Vec<String>),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for RouteError {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(err)
    }
}

pub async fn resolve(db: &SqlitePool, requested: &str) -> Result<Route, RouteError> {
    let requested = requested.trim();
    let not_found = || RouteError::NotFound(requested.to_owned());
    let rows: Vec<(i64, String, String, String)> = if let Some(model) = requested.strip_prefix("chatgpt/") {
        sqlx::query_as(
            "SELECT id, source, upstream_id, capabilities FROM models
             WHERE source = 'chatgpt' AND upstream_id = ? AND enabled = 1",
        )
        .bind(model)
        .fetch_all(db)
        .await?
    } else {
        // A bare name must match exactly one enabled model.
        sqlx::query_as("SELECT id, source, upstream_id, capabilities FROM models WHERE upstream_id = ? AND enabled = 1")
            .bind(requested)
            .fetch_all(db)
            .await?
    };
    match rows.as_slice() {
        [] => Err(not_found()),
        [(model_id, source, upstream_id, capabilities)] => Ok(Route {
            requested: requested.to_owned(),
            qualified: format!("{source}/{upstream_id}"),
            upstream_model: upstream_id.clone(),
            model_id: *model_id,
            capabilities: serde_json::from_str(capabilities).unwrap_or_default(),
        }),
        many => Err(RouteError::Ambiguous(
            requested.to_owned(),
            many.iter().map(|(_, source, id, _)| format!("{source}/{id}")).collect(),
        )),
    }
}

/// True when the key allows the route. An empty list allows all models. A pattern may match
/// the name the client sent or the qualified name; `*` matches any text.
pub fn allowed(patterns: &[String], route: &Route) -> bool {
    patterns.is_empty()
        || patterns
            .iter()
            .any(|p| glob(p, &route.requested) || glob(p, &route.qualified))
}

fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || !text[first.len()..].ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}
