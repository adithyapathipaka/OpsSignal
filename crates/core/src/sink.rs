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
