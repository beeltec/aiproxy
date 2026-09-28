//! Instance settings, edited on the Settings page.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Settings {
    /// IANA time zone, for example `Europe/Berlin`. Cron plans use it.
    pub time_zone: String,
    pub refresh: RefreshSchedule,
    pub failover: Failover,
}

/// Scheduled token refresh of the ChatGPT accounts.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RefreshSchedule {
    pub enabled: bool,
    /// 5-field cron: minute hour day-of-month month day-of-week.
    pub cron: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Failover {
    pub enabled: bool,
    /// Switch to the next account when its 5-hour or weekly usage reaches this percentage.
    pub threshold_percent: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            time_zone: "UTC".into(),
            refresh: RefreshSchedule {
                enabled: true,
                cron: "0 0 * * *".into(),
            },
            failover: Failover {
                enabled: false,
                threshold_percent: 95,
            },
        }
    }
}

const KEY: &str = "instance";

pub async fn load(db: &SqlitePool) -> Result<Settings, sqlx::Error> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(KEY)
        .fetch_optional(db)
        .await?;
    Ok(value
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default())
}

pub async fn save(db: &SqlitePool, settings: &Settings) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(KEY)
    .bind(serde_json::to_string(settings).expect("settings serialize"))
    .execute(db)
    .await?;
    Ok(())
}

impl Settings {
    /// Returns a readable message for the first invalid value.
    pub fn validate(&self) -> Result<(), String> {
        time_zone(&self.time_zone)?;
        parse_cron(&self.refresh.cron)?;
        if !(1..=100).contains(&self.failover.threshold_percent) {
            return Err("The failover threshold must be 1 to 100 percent.".into());
        }
        Ok(())
    }

    pub fn tz(&self) -> Tz {
        time_zone(&self.time_zone).unwrap_or(Tz::UTC)
    }
}

pub fn time_zone(name: &str) -> Result<Tz, String> {
    name.parse::<Tz>()
        .map_err(|_| format!("`{name}` is not a known time zone. Use a name such as Europe/Berlin or UTC."))
}

/// Parses a cron plan with exactly 5 fields.
pub fn parse_cron(expr: &str) -> Result<Cron, String> {
    let fields = expr.split_whitespace().count();
    if fields != 5 {
        return Err(format!(
            "A cron plan has 5 fields (minute hour day month weekday), not {fields}."
        ));
    }
    expr.parse::<Cron>()
        .map_err(|err| format!("The cron plan is not valid: {err}"))
}

/// The next `count` run times after `after`, computed in the time zone `tz`.
pub fn next_runs(cron: &Cron, tz: Tz, after: DateTime<Utc>, count: usize) -> Vec<DateTime<Utc>> {
    cron.iter_after(after.with_timezone(&tz))
        .take(count)
        .map(|at| at.with_timezone(&Utc))
        .collect()
}
