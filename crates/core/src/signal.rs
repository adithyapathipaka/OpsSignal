use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Severity is a closed enum, not a free string, so the routing engine
/// can match on it exhaustively instead of doing string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
    Critical,
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
}

fn default_metadata() -> serde_json::Value {
    serde_json::json!({})
}

/// Builder input — what callers actually construct. Notably missing
/// `id` and `timestamp`: those are not caller-settable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
}

impl Default for Severity {
    fn default() -> Self {
        Severity::Info
    }
}

impl Signal {
    pub fn from_input(input: SignalInput) -> Self {
        let timestamp = Utc::now();
        let dedup_key = input
            .dedup_key
            .clone()
            .unwrap_or_else(|| compute_dedup_key(&input));

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
pub enum SignalError {
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("delivery failed: {0}")]
    Delivery(String),
    #[error("sink write failed: {0}")]
    Sink(String),
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("internal error (recovered from panic): {0}")]
    Internal(String),
}

pub fn validate(input: &SignalInput) -> Result<(), SignalError> {
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
}
