use crate::delivery::DeliveryResult;
use crate::signal::{Signal, SignalError};
use rusqlite::Connection;
use std::sync::Mutex;

/// A sink is a write-only side effect of the pipeline — not a queryable
/// API the SDK exposes. Querying, dashboards, and aggregation are
/// explicitly out of scope here; that's the hosted control plane's job,
/// reading from the same store OSS users already populate.
pub trait SignalSink: Send + Sync {
    fn write(&self, signal: &Signal, result: &DeliveryResult) -> Result<(), SignalError>;
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
        Ok(SqliteSink {
            conn: Mutex::new(conn),
        })
    }
}

impl SignalSink for SqliteSink {
    fn write(&self, signal: &Signal, result: &DeliveryResult) -> Result<(), SignalError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| SignalError::Sink("sqlite connection mutex poisoned".to_string()))?;
        let payload =
            serde_json::to_string(signal).map_err(|e| SignalError::Sink(e.to_string()))?;
        conn.execute(
            "INSERT INTO signals (id, source, event_type, severity, title, environment, dedup_key, delivery_status, provider, error, payload_json, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
        }
    }

    #[test]
    fn write_stores_every_column() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        let signal = signal();
        sink.write(&signal, &failed()).expect("write");

        let conn = Connection::open(&db).expect("reopen");
        let row: Vec<Option<String>> = conn
            .query_row(
                "SELECT id, source, event_type, severity, title, environment, dedup_key, \
                 delivery_status, provider, error, payload_json, timestamp FROM signals",
                [],
                |r| (0..12).map(|i| r.get(i)).collect(),
            )
            .expect("one row");
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
    fn reopening_an_existing_database_keeps_its_rows() {
        let db = temp_db();
        SqliteSink::open(&db)
            .expect("open")
            .write(&signal(), &failed())
            .expect("write");
        let sink = SqliteSink::open(&db).expect("reopen is idempotent");
        sink.write(&signal(), &failed()).expect("second write");

        let count: i64 = Connection::open(&db)
            .expect("reopen")
            .query_row("SELECT COUNT(*) FROM signals", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 2);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn writing_the_same_signal_twice_is_a_sink_error() {
        let db = temp_db();
        let sink = SqliteSink::open(&db).expect("open");
        let signal = signal();
        sink.write(&signal, &failed()).expect("first write");

        let err = sink
            .write(&signal, &failed())
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
}
