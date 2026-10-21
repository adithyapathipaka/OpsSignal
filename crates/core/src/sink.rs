use crate::delivery::{DeliveryResult, ProviderOutcome};
use crate::signal::{Signal, SignalError};
use rusqlite::Connection;
use std::sync::Mutex;

/// A sink is a write-only side effect of the pipeline — not a queryable
/// API the SDK exposes. Querying, dashboards, and aggregation are
/// explicitly out of scope here; that's the hosted control plane's job,
/// reading from the same store OSS users already populate.
///
/// Each signal is written twice: `record` before delivery starts, with
/// status `pending`, and `complete` once the outcome is known. A signal
/// whose process exits mid-delivery therefore stays visible as pending
/// instead of vanishing.
pub trait SignalSink: Send + Sync {
    /// Stores a signal that is about to be delivered.
    fn record(&self, signal: &Signal) -> Result<(), SignalError>;

    /// Stores the delivery outcome, including `result.deliveries`. Must
    /// also work when `record` failed or never ran for this signal, so
    /// the outcome is never lost.
    fn complete(&self, signal: &Signal, result: &DeliveryResult) -> Result<(), SignalError>;

    /// Stores one provider's outcome while delivery is still running:
    /// `pending` before its first attempt, then `delivered` or `failed`.
    /// A fan-out cut short by a timeout or an exit then still shows
    /// which providers already have the signal. Sinks that only keep
    /// the overall outcome can leave this as is.
    fn provider_outcome(
        &self,
        _signal: &Signal,
        _outcome: &ProviderOutcome,
    ) -> Result<(), SignalError> {
        Ok(())
    }
}

/// Zero-config default sink. Always available, no setup required, so
/// "storage on by default" is actually true out of the box.
pub struct SqliteSink {
    conn: Mutex<Connection>,
}

impl SqliteSink {
    pub fn open(path: &str) -> Result<Self, SignalError> {
        let conn = Connection::open(path).map_err(|e| SignalError::Sink(e.to_string()))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS signals (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                event_type TEXT NOT NULL,
                severity TEXT NOT NULL,
                title TEXT NOT NULL,
                environment TEXT,
                dedup_key TEXT,
                delivery_status TEXT NOT NULL,
                provider TEXT,
                error TEXT,
                payload_json TEXT NOT NULL,
                timestamp TEXT NOT NULL
            )",
            [],
        )
        .map_err(|e| SignalError::Sink(e.to_string()))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_signals_dedup ON signals(dedup_key)",
            [],
        )
        .map_err(|e| SignalError::Sink(e.to_string()))?;
        // One row per (signal, provider). Added in beta.2; an older
        // database gets the table on its next open.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS deliveries (
                signal_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                status TEXT NOT NULL,
                error TEXT,
                attempts INTEGER NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (signal_id, provider)
            )",
            [],
        )
        .map_err(|e| SignalError::Sink(e.to_string()))?;
        Ok(SqliteSink {
            conn: Mutex::new(conn),
        })
    }
}

impl SqliteSink {
    fn insert(
        &self,
        signal: &Signal,
        result: &DeliveryResult,
        on_conflict: &str,
    ) -> Result<(), SignalError> {
        let conn = self.lock()?;
        let payload =
            serde_json::to_string(signal).map_err(|e| SignalError::Sink(e.to_string()))?;
        conn.execute(
            &format!(
                "INSERT INTO signals (id, source, event_type, severity, title, environment, dedup_key, delivery_status, provider, error, payload_json, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) {on_conflict}"
            ),
            rusqlite::params![
                signal.id.to_string(),
                signal.source,
                signal.event_type,
                signal.severity.to_string(),
                signal.title,
                signal.environment,
                signal.dedup_key,
                result.status.to_string(),
                result.provider.clone(),
                result.error.clone(),
                payload,
                signal.timestamp.to_rfc3339(),
            ],
        )
        .map_err(|e| SignalError::Sink(e.to_string()))?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SignalError> {
        self.conn
            .lock()
            .map_err(|_| SignalError::Sink("sqlite connection mutex poisoned".to_string()))
    }
}

fn upsert_outcome(
    conn: &Connection,
    signal: &Signal,
    outcome: &ProviderOutcome,
) -> Result<(), SignalError> {
    conn.execute(
        "INSERT INTO deliveries (signal_id, provider, status, error, attempts, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(signal_id, provider) DO UPDATE SET status = excluded.status, \
         error = excluded.error, attempts = excluded.attempts, updated_at = excluded.updated_at",
        rusqlite::params![
            signal.id.to_string(),
            outcome.provider,
            outcome.status.to_string(),
            outcome.error,
            outcome.attempts,
            chrono::Utc::now().to_rfc3339(),
        ],
    )
    .map_err(|e| SignalError::Sink(e.to_string()))?;
    Ok(())
}

impl SignalSink for SqliteSink {
    fn record(&self, signal: &Signal) -> Result<(), SignalError> {
        self.insert(signal, &DeliveryResult::pending(), "")
    }

    fn complete(&self, signal: &Signal, result: &DeliveryResult) -> Result<(), SignalError> {
        self.insert(
            signal,
            result,
            "ON CONFLICT(id) DO UPDATE SET delivery_status = excluded.delivery_status, \
             provider = excluded.provider, error = excluded.error",
        )?;
        let conn = self.lock()?;
        for outcome in &result.deliveries {
            upsert_outcome(&conn, signal, outcome)?;
        }
        Ok(())
    }

    fn provider_outcome(
        &self,
        signal: &Signal,
        outcome: &ProviderOutcome,
    ) -> Result<(), SignalError> {
        upsert_outcome(&*self.lock()?, signal, outcome)
    }
}

// NOTE for v0.3 roadmap: PostgresSink and S3Sink implement the same
// trait. Routing/retry logic never needs to change when a new sink is
// added — that boundary is the whole point.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::DeliveryStatus;
    use crate::signal::{Severity, SignalInput};

    fn temp_db() -> String {
        std::env::temp_dir()
            .join(format!("opssignal-sink-{}.db", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned()
    }

    fn signal() -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            severity: Severity::Error,
            environment: Some("prod".into()),
            metadata: Some(serde_json::json!({"dag": "nightly"})),
            ..Default::default()
        })
    }

    fn failed() -> DeliveryResult {
        DeliveryResult {
            status: DeliveryStatus::Failed,
            provider: None,
            error: Some("slack webhook returned status 500".into()),
            attempts: 3,
            deliveries: vec![],
        }
    }

    fn columns(db: &str) -> Vec<Option<String>> {
        Connection::open(db)
            .expect("reopen")
            .query_row(
                "SELECT id, source, event_type, severity, title, environment, dedup_key, \
                 delivery_status, provider, error, payload_json, timestamp FROM signals",
                [],
                |r| (0..12).map(|i| r.get(i)).collect(),
            )
            .expect("one row")
    }

    #[test]
    fn complete_stores_every_column() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        let signal = signal();
        sink.complete(&signal, &failed()).expect("complete");

        let row = columns(&db);
        let col = |i: usize| row[i].as_deref();

        assert_eq!(col(0), Some(signal.id.to_string().as_str()));
        assert_eq!(col(1), Some("airflow"));
        assert_eq!(col(2), Some("task_failed"));
        assert_eq!(col(3), Some("error"));
        assert_eq!(col(4), Some("dbt model failed"));
        assert_eq!(col(5), Some("prod"));
        assert_eq!(col(6), signal.dedup_key.as_deref());
        assert_eq!(col(7), Some("failed"));
        assert_eq!(col(8), None);
        assert_eq!(col(9), Some("slack webhook returned status 500"));
        let payload: serde_json::Value =
            serde_json::from_str(col(10).expect("payload")).expect("payload is JSON");
        assert_eq!(payload["metadata"]["dag"], "nightly");
        assert_eq!(col(11), Some(signal.timestamp.to_rfc3339().as_str()));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn record_stores_the_signal_as_pending() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        sink.record(&signal()).expect("record");

        let row = columns(&db);
        assert_eq!(row[7].as_deref(), Some("pending"));
        assert_eq!(row[8], None);
        assert_eq!(row[9], None);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn complete_updates_the_recorded_row_in_place() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        let signal = signal();
        sink.record(&signal).expect("record");
        let delivered = DeliveryResult {
            status: DeliveryStatus::Delivered,
            provider: Some("slack".into()),
            error: None,
            attempts: 1,
            deliveries: vec![],
        };
        sink.complete(&signal, &delivered).expect("complete");

        let conn = Connection::open(&db).expect("reopen");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM signals", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1);
        let row = columns(&db);
        assert_eq!(row[7].as_deref(), Some("delivered"));
        assert_eq!(row[8].as_deref(), Some("slack"));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn reopening_an_existing_database_keeps_its_rows() {
        let db = temp_db();
        SqliteSink::open(&db)
            .expect("open")
            .record(&signal())
            .expect("record");
        let sink = SqliteSink::open(&db).expect("reopen is idempotent");
        sink.record(&signal()).expect("second record");

        let count: i64 = Connection::open(&db)
            .expect("reopen")
            .query_row("SELECT COUNT(*) FROM signals", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 2);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn recording_the_same_signal_twice_is_a_sink_error() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        let signal = signal();
        sink.record(&signal).expect("first record");

        let err = sink
            .record(&signal)
            .expect_err("duplicate id violates the primary key");
        assert!(matches!(err, SignalError::Sink(_)));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn unopenable_path_is_a_sink_error() {
        let err = SqliteSink::open("/nonexistent-opssignal-dir/sub/signals.db")
            .err()
            .expect("missing parent directory must fail");
        assert!(matches!(err, SignalError::Sink(_)));
    }

    fn outcome(provider: &str, status: DeliveryStatus, attempts: u32) -> ProviderOutcome {
        ProviderOutcome {
            provider: provider.into(),
            status,
            error: (status == DeliveryStatus::Failed).then(|| format!("{provider} is down")),
            attempts,
        }
    }

    fn delivery_rows(db: &str) -> Vec<(String, String, Option<String>, u32)> {
        let conn = Connection::open(db).expect("reopen");
        let mut stmt = conn
            .prepare("SELECT provider, status, error, attempts FROM deliveries ORDER BY provider")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    }

    #[test]
    fn provider_outcomes_move_from_pending_to_their_result() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        let signal = signal();
        sink.record(&signal).expect("record");
        for p in ["pagerduty", "slack"] {
            sink.provider_outcome(&signal, &outcome(p, DeliveryStatus::Pending, 0))
                .expect("pending");
        }
        assert_eq!(
            delivery_rows(&db),
            vec![
                ("pagerduty".into(), "pending".into(), None, 0),
                ("slack".into(), "pending".into(), None, 0),
            ]
        );

        sink.provider_outcome(&signal, &outcome("slack", DeliveryStatus::Delivered, 1))
            .expect("slack done");
        sink.complete(
            &signal,
            &DeliveryResult {
                status: DeliveryStatus::Partial,
                provider: Some("slack".into()),
                error: Some("pagerduty: pagerduty is down".into()),
                attempts: 3,
                deliveries: vec![
                    outcome("slack", DeliveryStatus::Delivered, 1),
                    outcome("pagerduty", DeliveryStatus::Failed, 3),
                ],
            },
        )
        .expect("complete");

        assert_eq!(
            delivery_rows(&db),
            vec![
                (
                    "pagerduty".into(),
                    "failed".into(),
                    Some("pagerduty is down".into()),
                    3
                ),
                ("slack".into(), "delivered".into(), None, 1),
            ]
        );
        let row = columns(&db);
        assert_eq!(row[7].as_deref(), Some("partial"));
        assert_eq!(row[8].as_deref(), Some("slack"));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn opening_a_database_from_before_deliveries_adds_the_table() {
        let db = temp_db();
        Connection::open(&db)
            .expect("create")
            .execute_batch(
                "CREATE TABLE signals (id TEXT PRIMARY KEY, source TEXT NOT NULL, \
                 event_type TEXT NOT NULL, severity TEXT NOT NULL, title TEXT NOT NULL, \
                 environment TEXT, dedup_key TEXT, delivery_status TEXT NOT NULL, \
                 provider TEXT, error TEXT, payload_json TEXT NOT NULL, timestamp TEXT NOT NULL);",
            )
            .expect("old schema");
        let sink = SqliteSink::open(&db).expect("open old database");
        let signal = signal();
        sink.provider_outcome(&signal, &outcome("slack", DeliveryStatus::Delivered, 1))
            .expect("write to the new table");
        assert_eq!(delivery_rows(&db).len(), 1);
        let _ = std::fs::remove_file(&db);
    }
}
