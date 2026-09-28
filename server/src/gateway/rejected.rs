use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::db::now;

/// Why the gateway refused a request before any work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    NoKey,
    BadKey,
    Revoked,
    Expired,
    RateLimited,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoKey => "no_key",
            Self::BadKey => "bad_key",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Counts refused requests in memory. `flush` writes them once per minute, so a flood of bad
/// requests causes no database load. Memory stays small: one number per hour and reason.
#[derive(Default)]
pub struct RejectedCounter {
    counts: Mutex<HashMap<(i64, Reason), i64>>,
}

impl RejectedCounter {
    pub fn count(&self, reason: Reason) {
        let hour = now() / 3600 * 3600;
        *self
            .counts
            .lock()
            .expect("rejected lock")
            .entry((hour, reason))
            .or_default() += 1;
    }

    pub async fn flush(&self, db: &SqlitePool) {
        let counts = std::mem::take(&mut *self.counts.lock().expect("rejected lock"));
        for ((hour, reason), count) in counts {
            let result = sqlx::query(
                "INSERT INTO rejected_requests (hour, reason, count) VALUES (?, ?, ?)
                 ON CONFLICT (hour, reason) DO UPDATE SET count = count + excluded.count",
            )
            .bind(hour)
            .bind(reason.as_str())
            .bind(count)
            .execute(db)
            .await;
            if let Err(err) = result {
                tracing::warn!(error = %err, "cannot save rejected request counts");
                // Keep the counts for the next flush.
                *self
                    .counts
                    .lock()
                    .expect("rejected lock")
                    .entry((hour, reason))
                    .or_default() += count;
            }
        }
    }

    /// Writes the counts every minute until the process stops.
    pub async fn run(&self, db: SqlitePool) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            self.flush(&db).await;
        }
    }
}
