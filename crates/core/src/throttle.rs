//! Route `dedup` and `rate_limit`.
//!
//! The state lives in a `throttle` table of the SQLite file the sink
//! writes to, not in memory, so it holds across every process on the
//! host that shares that file: a CLI run from cron, each Airflow task,
//! and a restarted service all see the same history. Coordinating hosts
//! with each other is out of scope for the embedded SDK.
//!
//! Each admitted send is one row: the route, the signal's dedup key and
//! the time. A route is keyed by a hash of its whole definition, so
//! editing a route starts its history afresh.
//!
//! Every storage error lets the signal through. Throttling exists to
//! cut noise; it must never be the reason an alert went missing.

use crate::routing::Route;
use crate::signal::{Signal, SignalError};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::Duration;

/// How long a process waits on another one holding the file's write
/// lock before giving up (and letting the signal through).
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct Throttle {
    conn: Mutex<Connection>,
}

/// The sends recorded for one signal, so they can be taken back if
/// delivery fails.
#[derive(Debug, Default)]
pub(crate) struct Admission {
    rows: Vec<(String, String, i64)>,
}

impl Throttle {
    pub(crate) fn open(path: &str) -> Result<Self, SignalError> {
        let err = |e: rusqlite::Error| SignalError::Sink(format!("throttle store: {e}"));
        let conn = Connection::open(path).map_err(err)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS throttle (
                route TEXT NOT NULL,
                dedup_key TEXT NOT NULL,
                sent_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_throttle_route ON throttle(route, sent_at);",
        )
        .map_err(err)?;
        Ok(Throttle {
            conn: Mutex::new(conn),
        })
    }

    /// Decides whether `route` may send `signal` at `now_ms` (Unix
    /// milliseconds) and, if so, records the send in `admission`.
    pub(crate) fn admit(
        &self,
        route: &Route,
        signal: &Signal,
        now_ms: i64,
        admission: &mut Admission,
    ) -> Result<(), String> {
        let key = signal.dedup_key.clone().unwrap_or_default();
        let route_key = route_key(route);
        match self.try_admit(route, &route_key, &key, now_ms) {
            Ok(Ok(())) => {
                admission.rows.push((route_key, key, now_ms));
                Ok(())
            }
            Ok(Err(reason)) => Err(reason),
            Err(e) => {
                tracing::error!("throttle check failed, sending anyway: {e}");
                Ok(())
            }
        }
    }

    fn try_admit(
        &self,
        route: &Route,
        route_key: &str,
        key: &str,
        now_ms: i64,
    ) -> rusqlite::Result<Result<(), String>> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // IMMEDIATE takes the write lock up front, so two processes
        // can't both see an empty window and both send.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(window) = route.dedup {
            let last: Option<i64> = tx
                .query_row(
                    "SELECT MAX(sent_at) FROM throttle WHERE route = ?1 AND dedup_key = ?2",
                    rusqlite::params![route_key, key],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(last) = last.filter(|&t| now_ms - t < millis(window.0)) {
                return Ok(Err(format!(
                    "duplicate of a signal sent {} ago (dedup {window})",
                    ago(now_ms - last)
                )));
            }
        }
        if let Some(limit) = route.rate_limit {
            let sent: i64 = tx.query_row(
                "SELECT COUNT(*) FROM throttle WHERE route = ?1 AND sent_at > ?2",
                rusqlite::params![route_key, now_ms - millis(limit.per.0)],
                |r| r.get(0),
            )?;
            if sent >= i64::from(limit.max) {
                return Ok(Err(format!(
                    "rate limit of {} per {} reached",
                    limit.max, limit.per
                )));
            }
        }

        tx.execute(
            "INSERT INTO throttle (route, dedup_key, sent_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![route_key, key, now_ms],
        )?;
        // Rows older than both windows can no longer decide anything.
        let keep = [route.dedup.map(|w| w.0), route.rate_limit.map(|l| l.per.0)]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or_default();
        tx.execute(
            "DELETE FROM throttle WHERE route = ?1 AND sent_at <= ?2",
            rusqlite::params![route_key, now_ms - millis(keep)],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Forgets the sends of a signal that no provider delivered, so
    /// the next occurrence of the incident is not suppressed as a
    /// duplicate of an alert nobody received.
    pub(crate) fn release(&self, admission: &Admission) {
        if admission.rows.is_empty() {
            return;
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        for (route, key, at) in &admission.rows {
            if let Err(e) = conn.execute(
                "DELETE FROM throttle WHERE rowid IN (SELECT rowid FROM throttle \
                 WHERE route = ?1 AND dedup_key = ?2 AND sent_at = ?3 LIMIT 1)",
                rusqlite::params![route, key, at],
            ) {
                tracing::error!("throttle release failed: {e}");
            }
        }
    }
}

fn route_key(route: &Route) -> String {
    let json = serde_json::to_string(route).unwrap_or_default();
    format!("{:x}", Sha256::digest(json.as_bytes()))
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn ago(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{DeliveryMode, RateLimit, RouteMatch, Window};
    use crate::signal::SignalInput;

    const SEC: i64 = 1000;

    fn route(dedup: Option<u64>, rate: Option<(u32, u64)>) -> Route {
        Route {
            match_rule: RouteMatch {
                severity: vec![],
                environment: None,
                source: None,
            },
            providers: vec!["pagerduty".into()],
            mode: DeliveryMode::Failover,
            dedup: dedup.map(|s| Window(Duration::from_secs(s))),
            rate_limit: rate.map(|(max, per)| RateLimit {
                max,
                per: Window(Duration::from_secs(per)),
            }),
        }
    }

    fn signal(title: &str) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: title.into(),
            ..Default::default()
        })
    }

    fn memory() -> Throttle {
        Throttle::open(":memory:").expect("open")
    }

    fn admit(t: &Throttle, route: &Route, signal: &Signal, at: i64) -> Result<(), String> {
        t.admit(route, signal, at, &mut Admission::default())
    }

    #[test]
    fn dedup_suppresses_repeats_inside_the_window_only() {
        let t = memory();
        let r = route(Some(600), None);
        let s = signal("disk full");
        assert!(admit(&t, &r, &s, 0).is_ok());
        let err = admit(&t, &r, &s, 120 * SEC).expect_err("duplicate");
        assert_eq!(err, "duplicate of a signal sent 2m ago (dedup 10m)");
        assert!(admit(&t, &r, &s, 599 * SEC).is_err());
        // The window counts from the last send, not the last repeat, so
        // a long incident re-alerts once per window.
        assert!(admit(&t, &r, &s, 600 * SEC).is_ok());
    }

    #[test]
    fn dedup_keys_are_independent() {
        let t = memory();
        let r = route(Some(600), None);
        assert!(admit(&t, &r, &signal("a"), 0).is_ok());
        assert!(admit(&t, &r, &signal("b"), SEC).is_ok());
        assert!(admit(&t, &r, &signal("a"), 2 * SEC).is_err());
    }

    #[test]
    fn routes_keep_separate_histories() {
        let t = memory();
        let s = signal("a");
        let mut other = route(Some(600), None);
        other.providers = vec!["slack".into()];
        assert!(admit(&t, &route(Some(600), None), &s, 0).is_ok());
        assert!(admit(&t, &other, &s, SEC).is_ok());
    }

    #[test]
    fn rate_limit_is_a_sliding_window_across_keys() {
        let t = memory();
        let r = route(None, Some((2, 60)));
        assert!(admit(&t, &r, &signal("a"), 0).is_ok());
        assert!(admit(&t, &r, &signal("b"), 10 * SEC).is_ok());
        let err = admit(&t, &r, &signal("c"), 20 * SEC).expect_err("limited");
        assert_eq!(err, "rate limit of 2 per 1m reached");
        // The first send leaves the window after 60s.
        assert!(admit(&t, &r, &signal("c"), 61 * SEC).is_ok());
        assert!(admit(&t, &r, &signal("d"), 62 * SEC).is_err());
    }

    #[test]
    fn suppressed_signals_do_not_use_up_the_rate_limit() {
        let t = memory();
        let r = route(Some(600), Some((2, 60)));
        let s = signal("a");
        assert!(admit(&t, &r, &s, 0).is_ok());
        for i in 1..10 {
            assert!(admit(&t, &r, &s, i * SEC).is_err());
        }
        assert!(admit(&t, &r, &signal("b"), 11 * SEC).is_ok());
    }

    #[test]
    fn released_send_no_longer_counts() {
        let t = memory();
        let r = route(Some(600), None);
        let s = signal("a");
        let mut admission = Admission::default();
        assert!(t.admit(&r, &s, 0, &mut admission).is_ok());
        t.release(&admission);
        assert!(admit(&t, &r, &s, SEC).is_ok());
    }

    #[test]
    fn history_is_shared_through_the_file() {
        // Two stores on one file stand in for two processes.
        let db = std::env::temp_dir()
            .join(format!("opssignal-throttle-{}.db", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let r = route(Some(600), None);
        let s = signal("a");
        assert!(admit(&Throttle::open(&db).expect("open"), &r, &s, 0).is_ok());
        assert!(admit(&Throttle::open(&db).expect("open"), &r, &s, SEC).is_err());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn old_rows_are_pruned() {
        let t = memory();
        let r = route(Some(60), None);
        for i in 0..5 {
            assert!(admit(&t, &r, &signal(&i.to_string()), i * 100 * SEC).is_ok());
        }
        let rows: i64 = t
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM throttle", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }
}
