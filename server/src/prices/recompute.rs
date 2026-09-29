//! Recalculates the cost of stored usage rows with the current prices. One job runs at a time.

use std::sync::Mutex;

use serde::Serialize;
use sqlx::{QueryBuilder, Sqlite};
use utoipa::ToSchema;

use super::{COST_INPUT_COLUMNS, CostInput};
use crate::db::now;
use crate::state::AppState;

const BATCH: i64 = 500;

#[derive(Clone, Debug, Default, Serialize, ToSchema)]
pub struct Status {
    pub running: bool,
    /// Unix seconds of the time range.
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// Rows done and rows in the range.
    pub done: i64,
    pub total: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Recompute(Mutex<Status>);

impl Recompute {
    pub fn status(&self) -> Status {
        self.0.lock().expect("recompute lock").clone()
    }

    fn update(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.0.lock().expect("recompute lock"));
    }
}

/// The rows of the range (and of the models, when some are given), from `after` on.
fn filtered(select: &str, from: i64, to: i64, models: &[String]) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(select);
    query.push(" FROM usage WHERE time >= ").push_bind(from);
    query.push(" AND time < ").push_bind(to);
    if !models.is_empty() {
        query.push(" AND resolved_model IN (");
        let mut list = query.separated(", ");
        for model in models {
            list.push_bind(model.clone());
        }
        query.push(")");
    }
    query
}

/// Starts a job for the rows in `[from, to)`. Returns false when a job already runs.
pub fn start(state: &AppState, from: i64, to: i64, models: Vec<String>) -> bool {
    {
        let mut status = state.recompute.0.lock().expect("recompute lock");
        if status.running {
            return false;
        }
        *status = Status {
            running: true,
            from: Some(from),
            to: Some(to),
            started_at: Some(now()),
            ..Status::default()
        };
    }
    let state = state.clone();
    tokio::spawn(async move {
        let result = run(&state, from, to, &models).await;
        state.recompute.update(|status| {
            status.running = false;
            status.finished_at = Some(now());
            if let Err(err) = result {
                tracing::error!(error = %err, "the cost recompute failed");
                status.error = Some("The recompute failed. The server log has the details.".into());
            }
        });
    });
    true
}

async fn run(state: &AppState, from: i64, to: i64, models: &[String]) -> Result<(), sqlx::Error> {
    let total: i64 = filtered("SELECT COUNT(*)", from, to, models)
        .build_query_scalar()
        .fetch_one(&state.db)
        .await?;
    state.recompute.update(|status| status.total = total);
    let book = state.prices.get();
    let mut after = 0_i64;
    loop {
        let mut query = filtered(&format!("SELECT id, {COST_INPUT_COLUMNS}"), from, to, models);
        query.push(" AND id > ").push_bind(after);
        query.push(" ORDER BY id LIMIT ").push_bind(BATCH);
        let rows: Vec<(i64, CostInput)> = query
            .build()
            .fetch_all(&state.db)
            .await?
            .iter()
            .map(|row| {
                use sqlx::{FromRow, Row};
                Ok((row.try_get("id")?, CostInput::from_row(row)?))
            })
            .collect::<Result<_, sqlx::Error>>()?;
        let Some((last, _)) = rows.last() else { break };
        after = *last;
        let mut tx = state.db.begin().await?;
        for (id, input) in &rows {
            let cost = book.cost(input);
            sqlx::query(
                "UPDATE usage SET cost_nano = ?, cost_complete = ?, cost_parts = ?, price_version_id = ? WHERE id = ?",
            )
            .bind(cost.nano)
            .bind(cost.complete)
            .bind(cost.parts_json())
            .bind(cost.version)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        let count = rows.len() as i64;
        state.recompute.update(|status| status.done += count);
    }
    Ok(())
}
