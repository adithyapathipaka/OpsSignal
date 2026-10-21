use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Severity is a closed enum, not a free string, so the routing engine
/// can match on it exhaustively instead of doing string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Success,
    Warning,
    Error,
    Critical,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Severity::Info => "info",
            Severity::Success => "success",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Critical => "critical",
        })
    }
}

impl std::str::FromStr for Severity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "info" => Ok(Severity::Info),
            "success" => Ok(Severity::Success),
            "warning" => Ok(Severity::Warning),
            "error" => Ok(Severity::Error),
            "critical" => Ok(Severity::Critical),
            other => Err(format!(
                "unknown severity {other:?}; expected one of: info, success, warning, error, critical"
            )),
        }
    }
}

/// What a signal does to the alert its `dedup_key` names. `trigger`
/// raises (or repeats) it; `acknowledge` and `resolve` act on an alert
/// already raised with the same key, on the providers that have it.
/// Added in v1.0.0-beta.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Trigger,
    Acknowledge,
    Resolve,
}

impl Action {
    /// Whether the signal acts on an existing alert rather than raising one.
    pub fn is_lifecycle(self) -> bool {
        self != Action::Trigger
    }
}

impl Action {
    /// How messages announce an `acknowledge` or `resolve`
    /// ("Acknowledged", "Resolved"); `None` for a trigger.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Action::Trigger => None,
            Action::Acknowledge => Some("Acknowledged"),
            Action::Resolve => Some("Resolved"),
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Action::Trigger => "trigger",
            Action::Acknowledge => "acknowledge",
            Action::Resolve => "resolve",
        })
    }
}

impl std::str::FromStr for Action {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "trigger" => Ok(Action::Trigger),
            "acknowledge" | "ack" => Ok(Action::Acknowledge),
            "resolve" => Ok(Action::Resolve),
            other => Err(format!(
                "unknown action {other:?}; expected one of: trigger, acknowledge, resolve"
            )),
        }
    }
}

/// The generic signal model. `id` and `timestamp` are always stamped by
/// the core (never trusted from the caller) so every downstream
/// component — sink, dedup, retry tracking — has a stable identity and
/// ordering guarantee regardless of which language binding produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: Uuid,
    pub source: String,
    pub service: Option<String>,
    pub component: Option<String>,
    pub event_type: String,
    pub severity: Severity,
    pub title: String,
    pub message: Option<String>,
    pub environment: Option<String>,
    pub team: Option<String>,
    pub run_id: Option<String>,
    pub correlation_id: Option<String>,
    pub trace_id: Option<String>,
    pub log_url: Option<String>,
    #[serde(default = "default_metadata")]
    pub metadata: serde_json::Value,
    pub timestamp: DateTime<Utc>,
    pub dedup_key: Option<String>,
    #[serde(default)]
    pub action: Action,
}

fn default_metadata() -> serde_json::Value {
    serde_json::json!({})
}

/// Builder input — what callers actually construct. Notably missing
/// `id` and `timestamp`: those are not caller-settable. Every field may
/// be left out of JSON input; [`validate`] reports the ones a signal
/// needs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SignalInput {
    pub source: String,
    pub service: Option<String>,
    pub component: Option<String>,
    pub event_type: String,
    pub severity: Severity,
    pub title: String,
    pub message: Option<String>,
    pub environment: Option<String>,
    pub team: Option<String>,
    pub run_id: Option<String>,
    pub correlation_id: Option<String>,
    pub trace_id: Option<String>,
    pub log_url: Option<String>,
    pub metadata: Option<serde_json::Value>,
    /// Caller may supply an explicit dedup key; if absent, core computes
    /// one from (source, event_type, title, environment).
    pub dedup_key: Option<String>,
    /// `trigger` unless set. An `acknowledge` or `resolve` needs the
    /// alert's dedup key, given or computed from the same fields.
    #[serde(default)]
    pub action: Action,
}

impl Signal {
    pub fn from_input(input: SignalInput) -> Self {
        let timestamp = Utc::now();
        let dedup_key = input.effective_dedup_key();

        Signal {
            id: Uuid::new_v4(),
            source: input.source,
            service: input.service,
            component: input.component,
            event_type: input.event_type,
            severity: input.severity,
            title: input.title,
            message: input.message,
            environment: input.environment,
            team: input.team,
            run_id: input.run_id,
            correlation_id: input.correlation_id,
            trace_id: input.trace_id,
            log_url: input.log_url,
            metadata: input.metadata.unwrap_or_else(default_metadata),
            timestamp,
            dedup_key: Some(dedup_key),
            action: input.action,
        }
    }
}

fn compute_dedup_key(input: &SignalInput) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.source.as_bytes());
    hasher.update(b"|");
    hasher.update(input.event_type.as_bytes());
    hasher.update(b"|");
    hasher.update(input.title.as_bytes());
    hasher.update(b"|");
    hasher.update(input.environment.as_deref().unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignalError {
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("delivery failed: {0}")]
    Delivery(String),
    #[error("sink write failed: {0}")]
    Sink(String),
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("internal error (recovered from panic): {0}")]
    Internal(String),
}

/// Checks the fields a signal needs. An `acknowledge` or `resolve` may
/// give just its `dedup_key`; the client fills in the rest (see
/// [`SignalInput::fill_lifecycle_defaults`]).
pub fn validate(input: &SignalInput) -> Result<(), SignalError> {
    if input.action.is_lifecycle() {
        match input.dedup_key.as_deref() {
            Some(key) if !key.trim().is_empty() => return Ok(()),
            Some(_) => {
                return Err(SignalError::Validation(format!(
                    "dedup_key must not be blank for {}",
                    input.action
                )))
            }
            // Falls through: the key is computed from the fields below.
            None => {}
        }
    }
    if input.source.trim().is_empty() {
        return Err(SignalError::Validation("source must not be empty".into()));
    }
    if input.event_type.trim().is_empty() {
        return Err(SignalError::Validation(
            "event_type must not be empty".into(),
        ));
    }
    if input.title.trim().is_empty() {
        return Err(SignalError::Validation("title must not be empty".into()));
    }
    Ok(())
}

impl SignalInput {
    /// Gives an `acknowledge` or `resolve` sent with only its dedup key
    /// the fields every provider needs: `source` "opssignal",
    /// `event_type` the action, and `title` the alert's (`known_title`,
    /// from when it was raised) or else the key.
    pub fn fill_lifecycle_defaults(&mut self, known_title: Option<&str>) {
        if self.source.trim().is_empty() {
            self.source = "opssignal".into();
        }
        if self.event_type.trim().is_empty() {
            self.event_type = self.action.to_string();
        }
        if self.title.trim().is_empty() {
            self.title = known_title
                .map(str::to_string)
                .or_else(|| self.dedup_key.clone())
                .unwrap_or_default();
        }
    }

    /// The dedup key this input's signal gets: the given one, or the
    /// one computed from its fields.
    pub fn effective_dedup_key(&self) -> String {
        self.dedup_key
            .clone()
            .unwrap_or_else(|| compute_dedup_key(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_key_is_stable_for_identical_input() {
        let input = SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            environment: Some("prod".into()),
            severity: Severity::Warning,
            ..Default::default()
        };
        let k1 = compute_dedup_key(&input);
        let k2 = compute_dedup_key(&input);
        assert_eq!(k1, k2);
    }

    #[test]
    fn validate_rejects_empty_title() {
        let input = SignalInput {
            source: "x".into(),
            event_type: "y".into(),
            title: "".into(),
            severity: Severity::Info,
            ..Default::default()
        };
        assert!(validate(&input).is_err());
    }

    #[test]
    fn severity_display_is_lowercase() {
        assert_eq!(Severity::Info.to_string(), "info");
        assert_eq!(Severity::Critical.to_string(), "critical");
        assert_eq!(Severity::Warning.to_string(), "warning");
    }

    #[test]
    fn severity_from_str_roundtrips() {
        for s in &["info", "success", "warning", "error", "critical"] {
            let parsed: Severity = s.parse().expect("valid severity");
            assert_eq!(parsed.to_string(), *s);
        }
    }

    #[test]
    fn severity_from_str_is_case_insensitive() {
        assert_eq!("WARNING".parse::<Severity>().unwrap(), Severity::Warning);
        assert_eq!("Error".parse::<Severity>().unwrap(), Severity::Error);
    }

    #[test]
    fn severity_from_str_rejects_unknown() {
        assert!("bogus".parse::<Severity>().is_err());
    }

    fn valid_input() -> SignalInput {
        SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            environment: Some("prod".into()),
            ..Default::default()
        }
    }

    #[test]
    fn validate_accepts_minimal_input() {
        assert!(validate(&valid_input()).is_ok());
    }

    #[test]
    fn validate_rejects_blank_required_fields() {
        for field in ["source", "event_type", "title"] {
            let mut input = valid_input();
            match field {
                "source" => input.source = " \t".into(),
                "event_type" => input.event_type = String::new(),
                _ => input.title = "\n".into(),
            }
            let err = validate(&input).expect_err("blank field must be rejected");
            assert!(matches!(err, SignalError::Validation(_)));
            assert!(err.to_string().contains(field), "{err} should name {field}");
        }
    }

    #[test]
    fn id_and_timestamp_are_core_generated_even_if_the_payload_carries_them() {
        // CLAUDE.md hard rule 4: callers cannot supply id or timestamp.
        let caller_id = "00000000-0000-0000-0000-000000000001";
        let json = format!(
            r#"{{"id":"{caller_id}","timestamp":"2000-01-01T00:00:00Z","source":"ci","event_type":"build","title":"t","severity":"info"}}"#
        );
        let input: SignalInput = serde_json::from_str(&json).expect("input parses");

        let before = Utc::now();
        let signal = Signal::from_input(input);
        let after = Utc::now();

        assert_ne!(signal.id.to_string(), caller_id);
        assert!(signal.timestamp >= before && signal.timestamp <= after);
    }

    #[test]
    fn each_signal_gets_a_unique_id() {
        let a = Signal::from_input(valid_input());
        let b = Signal::from_input(valid_input());
        assert_ne!(a.id, b.id);
        assert_eq!(a.dedup_key, b.dedup_key);
    }

    #[test]
    fn caller_supplied_dedup_key_is_kept() {
        let mut input = valid_input();
        input.dedup_key = Some("my-key".into());
        assert_eq!(
            Signal::from_input(input).dedup_key.as_deref(),
            Some("my-key")
        );
    }

    #[test]
    fn dedup_key_changes_with_each_identifying_field() {
        let base = compute_dedup_key(&valid_input());
        let variants: [fn(&mut SignalInput); 4] = [
            |i| i.source = "jenkins".into(),
            |i| i.event_type = "task_retried".into(),
            |i| i.title = "other".into(),
            |i| i.environment = None,
        ];
        for change in variants {
            let mut input = valid_input();
            change(&mut input);
            assert_ne!(compute_dedup_key(&input), base);
        }
        // Severity and message are deliberately not part of the key.
        let mut input = valid_input();
        input.severity = Severity::Critical;
        input.message = Some("details".into());
        assert_eq!(compute_dedup_key(&input), base);
    }

    #[test]
    fn missing_metadata_defaults_to_empty_object() {
        assert_eq!(
            Signal::from_input(valid_input()).metadata,
            serde_json::json!({})
        );
    }

    #[test]
    fn action_defaults_to_trigger_and_parses() {
        let input: SignalInput = serde_json::from_str(
            r#"{"source":"ci","event_type":"e","title":"t","severity":"info"}"#,
        )
        .unwrap();
        assert_eq!(input.action, Action::Trigger);
        let input: SignalInput = serde_json::from_str(
            r#"{"source":"ci","event_type":"e","title":"t","severity":"info","action":"resolve"}"#,
        )
        .unwrap();
        assert_eq!(Signal::from_input(input).action, Action::Resolve);
        assert_eq!("ACK".parse::<Action>().unwrap(), Action::Acknowledge);
        assert!("close".parse::<Action>().is_err());
    }

    #[test]
    fn lifecycle_signal_needs_only_a_dedup_key() {
        let mut input = SignalInput {
            action: Action::Resolve,
            dedup_key: Some("airflow:etl.load".into()),
            ..Default::default()
        };
        assert!(validate(&input).is_ok());
        input.fill_lifecycle_defaults(Some("Load failed"));
        assert_eq!(input.source, "opssignal");
        assert_eq!(input.event_type, "resolve");
        assert_eq!(input.title, "Load failed");

        let mut bare = SignalInput {
            action: Action::Acknowledge,
            dedup_key: Some("k".into()),
            ..Default::default()
        };
        bare.fill_lifecycle_defaults(None);
        assert_eq!(bare.title, "k");

        // Without a key, a resolve needs the fields the key is computed from.
        let keyless = SignalInput {
            action: Action::Resolve,
            ..Default::default()
        };
        assert!(validate(&keyless).is_err());
        let blank_key = SignalInput {
            action: Action::Resolve,
            dedup_key: Some("  ".into()),
            ..Default::default()
        };
        assert!(validate(&blank_key).is_err());
    }
}
