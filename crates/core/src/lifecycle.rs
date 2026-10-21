//! Alert lifecycle: which providers hold an open alert for each dedup
//! key, so an `acknowledge` or `resolve` goes to the providers that got
//! the alert and nowhere else.
//!
//! The state lives in an `alerts` table of the SQLite file the sink
//! writes to, like the route throttle, so every process on the host
//! that shares the file sees it: an Airflow failure callback raises the
//! alert and the success callback of a later try resolves it. Hosts
//! that don't share the file still resolve on PagerDuty and Opsgenie,
//! which match the dedup key themselves (see
//! [`crate::delivery::Provider::resolves_by_key`]).
//!
//! One row per (dedup key, provider): raised `open`, then
//! `acknowledged` or `resolved`. A trigger after a resolve opens it
//! again. Every storage error is logged and ignored: the lifecycle
//! record must never be the reason an alert went missing.

use crate::delivery::{DeliveryResult, DeliveryStatus};
use crate::signal::{Action, Signal, SignalError};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// How long a process waits on another one holding the file's write
/// lock before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct AlertBook {
    conn: Mutex<Connection>,
}

/// A provider holding an alert that is not resolved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenAlert {
    pub provider: String,
    /// What the provider returned when it delivered the alert, such as
    /// the Slack channel and message timestamp.
    pub alert_ref: Option<String>,
    pub title: String,
}

impl AlertBook {
    pub(crate) fn open(path: &str) -> Result<Self, SignalError> {
        let err = |e: rusqlite::Error| SignalError::Sink(format!("alerts store: {e}"));
        let conn = Connection::open(path).map_err(err)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(err)?;
        // Added in beta.3; an older database gets the table on its next
        // open.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS alerts (
                dedup_key TEXT NOT NULL,
                provider TEXT NOT NULL,
                state TEXT NOT NULL,
                alert_ref TEXT,
                title TEXT NOT NULL,
                signal_id TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (dedup_key, provider)
            );",
        )
        .map_err(err)?;
        Ok(AlertBook {
            conn: Mutex::new(conn),
        })
    }

    /// The providers holding an unresolved alert for `dedup_key`, in
    /// the order they got it.
    pub(crate) fn open_alerts(&self, dedup_key: &str) -> Vec<OpenAlert> {
        let query = || -> rusqlite::Result<Vec<OpenAlert>> {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let mut stmt = conn.prepare(
                "SELECT provider, alert_ref, title FROM alerts
                 WHERE dedup_key = ?1 AND state != 'resolved' ORDER BY rowid",
            )?;
            let rows = stmt.query_map([dedup_key], |r| {
                Ok(OpenAlert {
                    provider: r.get(0)?,
                    alert_ref: r.get(1)?,
                    title: r.get(2)?,
                })
            })?;
            rows.collect()
        };
        query().unwrap_or_else(|e| {
            tracing::error!("alerts store read failed: {e}");
            Vec::new()
        })
    }

    /// Records what delivering `signal` did: each provider that got a
    /// trigger now holds an open alert (with the reference it returned
    /// in `refs`), and each that got an `acknowledge` or `resolve` moves
    /// its alert to that state.
    pub(crate) fn record(
        &self,
        signal: &Signal,
        result: &DeliveryResult,
        refs: &HashMap<String, String>,
    ) {
        let Some(key) = signal.dedup_key.as_deref() else {
            return;
        };
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let now = chrono::Utc::now().to_rfc3339();
        let delivered = result
            .deliveries
            .iter()
            .filter(|o| o.status == DeliveryStatus::Delivered);
        for outcome in delivered {
            let written = match signal.action {
                Action::Trigger => conn.execute(
                    "INSERT INTO alerts (dedup_key, provider, state, alert_ref, title, signal_id, updated_at)
                     VALUES (?1, ?2, 'open', ?3, ?4, ?5, ?6)
                     ON CONFLICT(dedup_key, provider) DO UPDATE SET state = 'open',
                     alert_ref = excluded.alert_ref, title = excluded.title,
                     signal_id = excluded.signal_id, updated_at = excluded.updated_at",
                    rusqlite::params![
                        key,
                        outcome.provider,
                        refs.get(&outcome.provider),
                        signal.title,
                        signal.id.to_string(),
                        now,
                    ],
                ),
                Action::Acknowledge | Action::Resolve => conn.execute(
                    "UPDATE alerts SET state = ?3, updated_at = ?4
                     WHERE dedup_key = ?1 AND provider = ?2",
                    rusqlite::params![
                        key,
                        outcome.provider,
                        state_after(signal.action),
                        now,
                    ],
                ),
            };
            if let Err(e) = written {
                tracing::error!("alerts store write failed: {e}");
            }
        }
    }
}

fn state_after(action: Action) -> &'static str {
    match action {
        Action::Trigger => "open",
        Action::Acknowledge => "acknowledged",
        Action::Resolve => "resolved",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::ProviderOutcome;
    use crate::signal::SignalInput;

    fn temp_db() -> String {
        std::env::temp_dir()
            .join(format!("opssignal-alerts-{}.db", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned()
    }

    fn signal(action: Action) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "load failed".into(),
            dedup_key: Some("airflow:etl.load".into()),
            action,
            ..Default::default()
        })
    }

    fn delivered_to(providers: &[&str]) -> DeliveryResult {
        let mut result = DeliveryResult::pending();
        result.deliveries = providers
            .iter()
            .map(|p| ProviderOutcome {
                provider: p.to_string(),
                status: DeliveryStatus::Delivered,
                error: None,
                attempts: 1,
            })
            .collect();
        result
    }

    #[test]
    fn alert_is_open_until_resolved_and_reopens_on_a_new_trigger() {
        let db = temp_db();
        let book = AlertBook::open(&db).expect("open");
        let refs = HashMap::from([("slack".to_string(), "C1/1.2".to_string())]);
        book.record(
            &signal(Action::Trigger),
            &delivered_to(&["slack", "pagerduty"]),
            &refs,
        );
        let open = book.open_alerts("airflow:etl.load");
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].provider, "slack");
        assert_eq!(open[0].alert_ref.as_deref(), Some("C1/1.2"));
        assert_eq!(open[0].title, "load failed");
        assert_eq!(open[1].alert_ref, None);

        // Acknowledged is still open: it can be resolved.
        book.record(
            &signal(Action::Acknowledge),
            &delivered_to(&["pagerduty"]),
            &HashMap::new(),
        );
        assert_eq!(book.open_alerts("airflow:etl.load").len(), 2);

        book.record(
            &signal(Action::Resolve),
            &delivered_to(&["slack", "pagerduty"]),
            &HashMap::new(),
        );
        assert!(book.open_alerts("airflow:etl.load").is_empty());

        book.record(
            &signal(Action::Trigger),
            &delivered_to(&["pagerduty"]),
            &HashMap::new(),
        );
        let open = book.open_alerts("airflow:etl.load");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].provider, "pagerduty");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn failed_deliveries_change_nothing() {
        let db = temp_db();
        let book = AlertBook::open(&db).expect("open");
        let mut result = delivered_to(&["slack"]);
        result.deliveries[0].status = DeliveryStatus::Failed;
        book.record(&signal(Action::Trigger), &result, &HashMap::new());
        assert!(book.open_alerts("airflow:etl.load").is_empty());

        book.record(
            &signal(Action::Trigger),
            &delivered_to(&["slack"]),
            &HashMap::new(),
        );
        book.record(&signal(Action::Resolve), &result, &HashMap::new());
        assert_eq!(book.open_alerts("airflow:etl.load").len(), 1);
        let _ = std::fs::remove_file(&db);
    }
}
