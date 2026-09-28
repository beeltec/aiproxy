//! Resolves the model name of a request to an upstream model. Order: alias, `chatgpt/<model>`,
//! `<connection slug>/<model>`, then a bare model name that matches exactly one enabled model.

use serde_json::{Map, Value};
use sqlx::SqlitePool;

use crate::connections::Kind;

/// Where a request goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upstream {
    ChatGpt,
    Connection { id: i64, kind: Kind },
}

/// Defaults of an alias. They apply only when the client did not set the value.
#[derive(Clone, Debug)]
pub struct Alias {
    pub name: String,
    pub effort: Option<String>,
    pub fast: bool,
    pub summary: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Route {
    /// The name the client sent.
    pub requested: String,
    /// `chatgpt/<model>` or `<slug>/<model>`.
    pub qualified: String,
    /// The model id at the upstream.
    pub upstream_model: String,
    /// Row id in the `models` table.
    pub model_id: i64,
    /// Synced capabilities with the admin changes.
    pub capabilities: Value,
    pub upstream: Upstream,
    pub alias: Option<Alias>,
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

/// The admin changes win over the synced values, key by key.
pub fn effective_capabilities(capabilities: &Value, overrides: &Value) -> Value {
    let mut merged: Map<String, Value> = capabilities.as_object().cloned().unwrap_or_default();
    for (key, value) in overrides.as_object().into_iter().flatten() {
        merged.insert(key.clone(), value.clone());
    }
    Value::Object(merged)
}

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: i64,
    prefix: String,
    upstream_id: String,
    capabilities: String,
    capability_overrides: String,
    connection_id: Option<i64>,
    kind: Option<String>,
    enabled: bool,
}

macro_rules! select_models {
    ($rest:literal) => {
        concat!(
            "SELECT m.id, COALESCE(c.slug, 'chatgpt') AS prefix, m.upstream_id, m.capabilities,
                 m.capability_overrides, c.id AS connection_id, c.kind, m.enabled
             FROM models m LEFT JOIN connections c ON m.source = CAST(c.id AS TEXT)
             WHERE (m.source = 'chatgpt' OR c.id IS NOT NULL) AND ",
            $rest
        )
    };
}

impl ModelRow {
    fn route(self, requested: &str, alias: Option<Alias>) -> Route {
        let upstream = match (self.connection_id, self.kind.as_deref().and_then(Kind::parse)) {
            (Some(id), Some(kind)) => Upstream::Connection { id, kind },
            _ => Upstream::ChatGpt,
        };
        let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap_or_default();
        Route {
            requested: requested.to_owned(),
            qualified: format!("{}/{}", self.prefix, self.upstream_id),
            upstream_model: self.upstream_id,
            model_id: self.id,
            capabilities: effective_capabilities(&parse(&self.capabilities), &parse(&self.capability_overrides)),
            upstream,
            alias,
        }
    }
}

/// The model for a qualified name (`chatgpt/<model>` or `<slug>/<model>`), enabled or not.
async fn qualified(db: &SqlitePool, name: &str) -> Result<Option<ModelRow>, sqlx::Error> {
    let Some((prefix, model)) = name.split_once('/') else {
        return Ok(None);
    };
    sqlx::query_as(select_models!("COALESCE(c.slug, 'chatgpt') = ? AND m.upstream_id = ?"))
        .bind(prefix)
        .bind(model)
        .fetch_optional(db)
        .await
}

/// The model id and enabled flag of a qualified name.
pub async fn find_model(db: &SqlitePool, name: &str) -> Result<Option<(i64, bool)>, sqlx::Error> {
    Ok(qualified(db, name).await?.map(|row| (row.id, row.enabled)))
}

pub async fn resolve(db: &SqlitePool, requested: &str) -> Result<Route, RouteError> {
    let requested = requested.trim();
    let not_found = || RouteError::NotFound(requested.to_owned());

    let alias: Option<(String, Option<String>, bool, Option<String>)> =
        sqlx::query_as("SELECT target, default_effort, default_fast, default_summary FROM aliases WHERE name = ?")
            .bind(requested)
            .fetch_optional(db)
            .await?;
    if let Some((target, effort, fast, summary)) = alias {
        let row = qualified(db, &target)
            .await?
            .filter(|row| row.enabled)
            .ok_or_else(not_found)?;
        let alias = Alias {
            name: requested.to_owned(),
            effort,
            fast,
            summary,
        };
        return Ok(row.route(requested, Some(alias)));
    }

    // A known prefix selects the source. OpenRouter ids have a slash too, so an unknown first
    // segment means a bare name.
    if let Some(row) = qualified(db, requested).await? {
        return if row.enabled {
            Ok(row.route(requested, None))
        } else {
            Err(not_found())
        };
    }

    let rows: Vec<ModelRow> = sqlx::query_as(select_models!("m.upstream_id = ? AND m.enabled = 1"))
        .bind(requested)
        .fetch_all(db)
        .await?;
    let mut rows = rows.into_iter();
    match (rows.next(), rows.len()) {
        (None, _) => Err(not_found()),
        (Some(row), 0) => Ok(row.route(requested, None)),
        (Some(first), _) => {
            let mut options = vec![format!("{}/{}", first.prefix, first.upstream_id)];
            options.extend(rows.map(|row| format!("{}/{}", row.prefix, row.upstream_id)));
            Err(RouteError::Ambiguous(requested.to_owned(), options))
        }
    }
}

/// A name that clients can list.
pub struct Listed {
    pub name: String,
    pub display_name: String,
    /// The names that the allowlist checks.
    pub names: Vec<String>,
}

/// Enabled models as qualified names, and the aliases of enabled models.
pub async fn listed(db: &SqlitePool) -> Result<Vec<Listed>, sqlx::Error> {
    let models: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT COALESCE(c.slug, 'chatgpt'), m.upstream_id, m.display_name
         FROM models m LEFT JOIN connections c ON m.source = CAST(c.id AS TEXT)
         WHERE m.enabled = 1 AND (m.source = 'chatgpt' OR c.id IS NOT NULL)
         ORDER BY m.source <> 'chatgpt', 1, 2",
    )
    .fetch_all(db)
    .await?;
    let aliases: Vec<(String, String)> = sqlx::query_as("SELECT name, target FROM aliases ORDER BY name")
        .fetch_all(db)
        .await?;
    let mut out = Vec::new();
    for (name, target) in aliases {
        if let Some((_, id, _)) = models.iter().find(|(prefix, id, _)| format!("{prefix}/{id}") == target) {
            out.push(Listed {
                names: vec![name.clone(), target.clone(), id.clone()],
                display_name: target,
                name,
            });
        }
    }
    for (prefix, id, display_name) in models {
        let qualified = format!("{prefix}/{id}");
        out.push(Listed {
            names: vec![qualified.clone(), id.clone()],
            display_name: display_name.unwrap_or_else(|| id.clone()),
            name: qualified,
        });
    }
    Ok(out)
}

impl Route {
    /// The names that an allowlist pattern may match: the name the client sent, the alias,
    /// the qualified name and the upstream model id.
    pub fn names(&self) -> Vec<&str> {
        let mut names = vec![
            self.requested.as_str(),
            self.qualified.as_str(),
            self.upstream_model.as_str(),
        ];
        if let Some(alias) = &self.alias {
            names.push(&alias.name);
        }
        names
    }
}

/// True when the key allows one of the names. An empty list allows all models. `*` matches any
/// text.
pub fn allowed(patterns: &[String], names: &[&str]) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| names.iter().any(|name| glob(p, name)))
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
