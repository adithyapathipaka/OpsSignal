//! Route `group_by` and `group_wait`: one digest instead of a burst.
//!
//! A grouped route does not send a matching signal at once. It adds the
//! signal to the open group for its `group_by` values, and the first
//! signal of a group starts its `group_wait`. When that runs out, or the
//! client is flushed (as every binding does on the way out), the group
//! is sent as one digest: a signal of its own, built from the members.
//! A group of one is sent as that signal, unchanged.
//!
//! Groups live in the client's memory, so they collect signals sent
//! through one client, in one process. Grouping across processes or
//! hosts is out of scope for the embedded SDK. Every member is stored
//! by the sink before it is grouped, so a process that dies with a
//! group open leaves its members on record as `grouped`.

use crate::inflight::{Guard, InFlight};
use crate::routing::{Route, GROUP_FIELDS};
use crate::signal::{Severity, Signal, SignalInput};
use crate::throttle::Admission;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A group this full is sent at once, and the next signal opens a new
/// one. Bounds memory when a group_wait is long and signals are many.
pub const MAX_GROUP_SIZE: usize = 500;

/// How many members a digest's message lists before "and N more".
const LISTED_IN_MESSAGE: usize = 20;

/// How many members a digest lists in `metadata.digest.signals`.
const LISTED_IN_METADATA: usize = 100;

/// A signal held in a group.
pub(crate) struct Member {
    pub signal: Signal,
    /// Its `dedup` record on this route, released if the digest fails.
    pub admission: Admission,
    /// Whether the member's stored row waits on the digest (`grouped`)
    /// and should take its outcome. False when the member was also
    /// sent at once on another route; that outcome stays.
    pub owns_row: bool,
}

/// A group taken out of the open set, ready to send.
pub(crate) struct Closed {
    pub route: Route,
    pub by: Vec<(String, Option<String>)>,
    pub members: Vec<Member>,
    /// Counts the send as in flight from the moment the group closes,
    /// so a flush can't miss it.
    pub _guard: Guard,
}

struct Open {
    seq: u64,
    route: Route,
    by: Vec<(String, Option<String>)>,
    members: Vec<Member>,
}

/// What adding a signal did.
pub(crate) enum Added {
    /// Joined an open group.
    Joined,
    /// Opened a group: the caller closes it with [`Groups::take`]
    /// after `wait`.
    Opened {
        key: String,
        seq: u64,
        wait: Duration,
    },
    /// Filled a group, which must be sent now.
    Full(Closed),
}

/// The open groups of one client.
pub(crate) struct Groups {
    open: Mutex<HashMap<String, Open>>,
    next_seq: Mutex<u64>,
    in_flight: Arc<InFlight>,
}

impl Groups {
    pub(crate) fn new(in_flight: Arc<InFlight>) -> Self {
        Groups {
            open: Mutex::default(),
            next_seq: Mutex::new(0),
            in_flight,
        }
    }

    /// Adds `member` to its group on `route`, opening one if needed.
    pub(crate) fn add(&self, route: &Route, member: Member) -> Added {
        let by = group_values(route, &member.signal);
        let key = group_key(route, &by);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(group) = open.get_mut(&key) {
            group.members.push(member);
            if group.members.len() < MAX_GROUP_SIZE {
                return Added::Joined;
            }
            return match open.remove(&key) {
                Some(group) => Added::Full(self.close(group)),
                None => Added::Joined,
            };
        }
        let seq = {
            let mut next = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        open.insert(
            key.clone(),
            Open {
                seq,
                route: route.clone(),
                by,
                members: vec![member],
            },
        );
        Added::Opened {
            key,
            seq,
            wait: route.group_window().unwrap_or_default(),
        }
    }

    /// Closes the group `key` if it is still the one opened as `seq`
    /// (it may have been sent full or by a flush meanwhile).
    pub(crate) fn take(&self, key: &str, seq: u64) -> Option<Closed> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if open.get(key)?.seq != seq {
            return None;
        }
        open.remove(key).map(|group| self.close(group))
    }

    /// Closes every open group.
    pub(crate) fn take_all(&self) -> Vec<Closed> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.drain().map(|(_, group)| self.close(group)).collect()
    }

    fn close(&self, group: Open) -> Closed {
        Closed {
            route: group.route,
            by: group.by,
            members: group.members,
            _guard: self.in_flight.start(),
        }
    }
}

/// The values of a route's `group_by` fields for `signal`, in order.
/// A field the signal doesn't set is `None`.
pub(crate) fn group_values(route: &Route, signal: &Signal) -> Vec<(String, Option<String>)> {
    route
        .group_by
        .iter()
        .flatten()
        .map(|field| (field.clone(), field_value(signal, field)))
        .collect()
}

fn group_key(route: &Route, by: &[(String, Option<String>)]) -> String {
    format!(
        "{}\n{}",
        crate::throttle::route_key(route),
        serde_json::to_string(by).unwrap_or_default()
    )
}

/// Whether `field` can be used in `group_by`.
pub fn is_group_field(field: &str) -> bool {
    GROUP_FIELDS.contains(&field)
        || field
            .strip_prefix("metadata.")
            .is_some_and(|path| !path.is_empty() && path.split('.').all(|p| !p.is_empty()))
}

/// The value of a `group_by` field: a signal field, or a dotted path
/// into its metadata. Strings are taken as they are, other JSON values
/// as JSON text.
pub(crate) fn field_value(signal: &Signal, field: &str) -> Option<String> {
    if let Some(path) = field.strip_prefix("metadata.") {
        let mut value = &signal.metadata;
        for part in path.split('.') {
            value = value.get(part)?;
        }
        return match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
    }
    let text = |v: &Option<String>| v.clone();
    match field {
        "source" => Some(signal.source.clone()),
        "service" => text(&signal.service),
        "component" => text(&signal.component),
        "event_type" => Some(signal.event_type.clone()),
        "severity" => Some(signal.severity.to_string()),
        "title" => Some(signal.title.clone()),
        "environment" => text(&signal.environment),
        "team" => text(&signal.team),
        "run_id" => text(&signal.run_id),
        "correlation_id" => text(&signal.correlation_id),
        "dedup_key" => text(&signal.dedup_key),
        _ => None,
    }
}

fn rank(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Success => 1,
        Severity::Warning => 2,
        Severity::Error => 3,
        Severity::Critical => 4,
    }
}

/// The value every member shares, or `None` if they differ or none
/// sets it.
fn common<F>(members: &[Member], get: F) -> Option<String>
where
    F: Fn(&Signal) -> Option<&String>,
{
    let first = get(&members.first()?.signal)?;
    members
        .iter()
        .all(|m| get(&m.signal) == Some(first))
        .then(|| first.clone())
}

/// Builds the digest for a group of two or more. It is a signal like
/// any other, so templates, dedup and the sink treat it the usual way:
///
/// - `title`: the first member's title and how many more there are;
/// - `message`: one line per member, the first [`LISTED_IN_MESSAGE`];
/// - `severity`: the highest of the members;
/// - `source`, `environment` and the other fields: the members' value
///   when they all share one (`source` is `opssignal` otherwise);
/// - `metadata.digest`: `count`, the `group` values, `first_at`,
///   `last_at` and `signals` (id, title, severity, source, event_type,
///   timestamp of each member, the first [`LISTED_IN_METADATA`]).
pub(crate) fn digest(closed: &Closed) -> Signal {
    let members = &closed.members;
    let first = &members[0].signal;
    let count = members.len();
    let severity = members
        .iter()
        .map(|m| m.signal.severity)
        .max_by_key(|s| rank(*s))
        .unwrap_or_default();

    let mut lines: Vec<String> = members
        .iter()
        .take(LISTED_IN_MESSAGE)
        .map(|m| format!("- [{}] {}", m.signal.severity, m.signal.title))
        .collect();
    if count > LISTED_IN_MESSAGE {
        lines.push(format!("...and {} more", count - LISTED_IN_MESSAGE));
    }

    let signals: Vec<serde_json::Value> = members
        .iter()
        .take(LISTED_IN_METADATA)
        .map(|m| {
            serde_json::json!({
                "id": m.signal.id,
                "title": m.signal.title,
                "severity": m.signal.severity,
                "source": m.signal.source,
                "event_type": m.signal.event_type,
                "timestamp": m.signal.timestamp,
            })
        })
        .collect();
    let group: serde_json::Map<String, serde_json::Value> = closed
        .by
        .iter()
        .map(|(field, value)| (field.clone(), serde_json::json!(value)))
        .collect();
    let first_at = members.iter().map(|m| m.signal.timestamp).min();
    let last_at = members.iter().map(|m| m.signal.timestamp).max();

    let key = format!(
        "digest:{:x}",
        Sha256::digest(group_key(&closed.route, &closed.by).as_bytes())
    );

    Signal::from_input(SignalInput {
        source: common(members, |s| Some(&s.source)).unwrap_or_else(|| "opssignal".into()),
        service: common(members, |s| s.service.as_ref()),
        component: common(members, |s| s.component.as_ref()),
        event_type: common(members, |s| Some(&s.event_type)).unwrap_or_else(|| "digest".into()),
        severity,
        title: format!("{} (+{} more)", first.title, count - 1),
        message: Some(lines.join("\n")),
        environment: common(members, |s| s.environment.as_ref()),
        team: common(members, |s| s.team.as_ref()),
        run_id: common(members, |s| s.run_id.as_ref()),
        correlation_id: common(members, |s| s.correlation_id.as_ref()),
        trace_id: common(members, |s| s.trace_id.as_ref()),
        log_url: common(members, |s| s.log_url.as_ref()),
        metadata: Some(serde_json::json!({
            "digest": {
                "count": count,
                "group": group,
                "first_at": first_at,
                "last_at": last_at,
                "signals": signals,
            }
        })),
        dedup_key: Some(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{DeliveryMode, RouteMatch, Window};

    fn route(by: &[&str], wait: Option<u64>) -> Route {
        Route {
            match_rule: RouteMatch {
                severity: vec![],
                environment: None,
                source: None,
            },
            providers: vec!["slack".into()],
            mode: DeliveryMode::Failover,
            dedup: None,
            rate_limit: None,
            group_by: Some(by.iter().map(|s| s.to_string()).collect()),
            group_wait: wait.map(|s| Window(Duration::from_secs(s))),
        }
    }

    fn signal(title: &str, severity: Severity, run: &str) -> Signal {
        Signal::from_input(SignalInput {
            source: "dbt".into(),
            event_type: "dbt_test_failed".into(),
            title: title.into(),
            severity,
            environment: Some("prod".into()),
            run_id: Some(run.into()),
            metadata: Some(serde_json::json!({ "invocation": { "id": run }, "n": 3 })),
            ..Default::default()
        })
    }

    fn member(signal: Signal) -> Member {
        Member {
            signal,
            admission: Admission::default(),
            owns_row: true,
        }
    }

    fn groups() -> Groups {
        Groups::new(Arc::default())
    }

    #[test]
    fn fields_and_metadata_paths_resolve() {
        let s = signal("t", Severity::Error, "abc");
        assert_eq!(field_value(&s, "run_id").as_deref(), Some("abc"));
        assert_eq!(field_value(&s, "severity").as_deref(), Some("error"));
        assert_eq!(field_value(&s, "team"), None);
        assert_eq!(
            field_value(&s, "metadata.invocation.id").as_deref(),
            Some("abc")
        );
        assert_eq!(field_value(&s, "metadata.n").as_deref(), Some("3"));
        assert_eq!(field_value(&s, "metadata.missing"), None);
    }

    #[test]
    fn group_fields_are_checked() {
        for ok in ["source", "run_id", "metadata.dag_id", "metadata.a.b"] {
            assert!(is_group_field(ok), "{ok}");
        }
        for bad in [
            "sorce",
            "metadata.",
            "metadata",
            "metadata.a..b",
            "id",
            "timestamp",
        ] {
            assert!(!is_group_field(bad), "{bad}");
        }
    }

    #[test]
    fn same_values_join_and_different_values_open_their_own_group() {
        let g = groups();
        let r = route(&["run_id"], Some(60));
        assert!(matches!(
            g.add(&r, member(signal("a", Severity::Error, "1"))),
            Added::Opened { .. }
        ));
        assert!(matches!(
            g.add(&r, member(signal("b", Severity::Error, "1"))),
            Added::Joined
        ));
        assert!(matches!(
            g.add(&r, member(signal("c", Severity::Error, "2"))),
            Added::Opened { .. }
        ));
        let mut closed = g.take_all();
        closed.sort_by_key(|c| c.members.len());
        assert_eq!(
            closed.iter().map(|c| c.members.len()).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(g.take_all().is_empty());
    }

    #[test]
    fn take_ignores_a_group_already_sent() {
        let g = groups();
        let r = route(&[], Some(60));
        let Added::Opened { key, seq, wait } = g.add(&r, member(signal("a", Severity::Info, "1")))
        else {
            panic!("expected a new group");
        };
        assert_eq!(wait, Duration::from_secs(60));
        assert_eq!(g.take_all().len(), 1);
        // The timer fires after a flush sent the group: nothing to do,
        // even when a new group has opened under the same key.
        g.add(&r, member(signal("b", Severity::Info, "1")));
        assert!(g.take(&key, seq).is_none());
        assert_eq!(g.take_all().len(), 1);
    }

    #[test]
    fn a_full_group_is_sent_at_once() {
        let g = groups();
        let r = route(&[], None);
        for i in 0..MAX_GROUP_SIZE - 1 {
            assert!(!matches!(
                g.add(&r, member(signal(&format!("s{i}"), Severity::Info, "1"))),
                Added::Full(_)
            ));
        }
        match g.add(&r, member(signal("last", Severity::Info, "1"))) {
            Added::Full(closed) => assert_eq!(closed.members.len(), MAX_GROUP_SIZE),
            _ => panic!("expected the group to be full"),
        }
        assert!(g.take_all().is_empty());
    }

    #[test]
    fn digest_summarises_its_members() {
        let g = groups();
        let r = route(&["run_id"], None);
        g.add(&r, member(signal("test a failed", Severity::Warning, "9")));
        g.add(&r, member(signal("test b failed", Severity::Error, "9")));
        let mut other = signal("test c failed", Severity::Warning, "9");
        other.environment = Some("staging".into());
        g.add(&r, member(other));
        let closed = g.take_all().pop().expect("group");

        let d = digest(&closed);
        assert_eq!(d.title, "test a failed (+2 more)");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.source, "dbt");
        assert_eq!(d.run_id.as_deref(), Some("9"));
        assert_eq!(d.environment, None, "members disagree");
        assert_eq!(
            d.message.as_deref(),
            Some("- [warning] test a failed\n- [error] test b failed\n- [warning] test c failed")
        );
        let meta = &d.metadata["digest"];
        assert_eq!(meta["count"], 3);
        assert_eq!(meta["group"]["run_id"], "9");
        assert_eq!(meta["signals"][1]["title"], "test b failed");
        assert!(d.dedup_key.expect("key").starts_with("digest:"));
        // Its own id, never a member's.
        assert!(closed.members.iter().all(|m| m.signal.id != d.id));
    }

    #[test]
    fn long_digest_message_is_cut() {
        let g = groups();
        let r = route(&[], None);
        for i in 0..25 {
            g.add(&r, member(signal(&format!("s{i}"), Severity::Info, "1")));
        }
        let d = digest(&g.take_all().pop().expect("group"));
        let message = d.message.expect("message");
        assert_eq!(message.lines().count(), LISTED_IN_MESSAGE + 1);
        assert!(message.ends_with("...and 5 more"));
        assert_eq!(d.metadata["digest"]["count"], 25);
    }
}
