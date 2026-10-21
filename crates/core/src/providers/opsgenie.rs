use crate::delivery::Provider;
use crate::signal::{Severity, Signal, SignalError};
use crate::template::{self, Templates};

/// Alert API endpoint for US-hosted accounts. EU accounts use
/// `https://api.eu.opsgenie.com/v2/alerts`, set through `url`.
pub const DEFAULT_ALERTS_URL: &str = "https://api.opsgenie.com/v2/alerts";

/// Opsgenie rejects longer values with a 422, which retrying cannot
/// fix, so they are cut to fit instead.
const MAX_MESSAGE_CHARS: usize = 130;
const MAX_ALIAS_CHARS: usize = 512;
const MAX_DESCRIPTION_CHARS: usize = 15000;
const MAX_SOURCE_CHARS: usize = 100;
const MAX_ENTITY_CHARS: usize = 512;
const MAX_TAGS: usize = 20;
const MAX_TAG_CHARS: usize = 50;

/// Creates an Opsgenie alert through the Alert API. The signal's
/// `dedup_key` becomes the alert `alias`, so a repeat of the same
/// signal adds to the open alert's count instead of opening a new one.
pub struct OpsgenieProvider {
    api_key: String,
    pub url: String,
    tags: Vec<String>,
    client: reqwest::Client,
    templates: Option<Templates>,
}

impl OpsgenieProvider {
    pub fn new(api_key: String, url: Option<String>, tags: Vec<String>) -> Self {
        OpsgenieProvider {
            api_key,
            url: url.unwrap_or_else(|| DEFAULT_ALERTS_URL.to_string()),
            tags,
            client: reqwest::Client::new(),
            templates: None,
        }
    }

    /// Uses `templates` for the parts they set; see [`crate::template`].
    pub fn with_templates(mut self, templates: Option<Templates>) -> Self {
        self.templates = templates;
        self
    }

    /// Opsgenie priorities run from P1 (critical) to P5
    /// (informational); P4 is left for people to assign by hand.
    fn priority_for(severity: Severity) -> &'static str {
        match severity {
            Severity::Critical => "P1",
            Severity::Error => "P2",
            Severity::Warning => "P3",
            Severity::Info | Severity::Success => "P5",
        }
    }

    fn alert_for(&self, signal: &Signal) -> serde_json::Value {
        // Opsgenie's details are a flat string-to-string map.
        let mut details = serde_json::Map::new();
        details.insert("signal_id".into(), signal.id.to_string().into());
        details.insert("event_type".into(), signal.event_type.clone().into());
        details.insert("severity".into(), signal.severity.to_string().into());
        details.insert("timestamp".into(), signal.timestamp.to_rfc3339().into());
        let optional = [
            ("environment", &signal.environment),
            ("service", &signal.service),
            ("component", &signal.component),
            ("team", &signal.team),
            ("run_id", &signal.run_id),
            ("correlation_id", &signal.correlation_id),
            ("trace_id", &signal.trace_id),
            ("log_url", &signal.log_url),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                details.insert(key.into(), value.clone().into());
            }
        }
        if let Some(metadata) = signal.metadata.as_object() {
            for (key, value) in metadata {
                let text = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                details
                    .entry(format!("metadata.{key}"))
                    .or_insert(text.into());
            }
        }

        let mut tags: Vec<String> = self.tags.clone();
        tags.push(signal.severity.to_string());
        if let Some(environment) = &signal.environment {
            tags.push(environment.clone());
        }
        let mut seen = std::collections::HashSet::new();
        let tags: Vec<String> = tags
            .into_iter()
            .map(|t| truncate(&t, MAX_TAG_CHARS))
            .filter(|t| !t.is_empty() && seen.insert(t.clone()))
            .take(MAX_TAGS)
            .collect();

        let message = template::render(&self.templates, "message", signal, &[])
            .unwrap_or_else(|| signal.title.clone());
        let mut alert = serde_json::json!({
            "message": truncate(&message, MAX_MESSAGE_CHARS),
            "source": truncate(&signal.source, MAX_SOURCE_CHARS),
            "priority": Self::priority_for(signal.severity),
            "tags": tags,
            "details": details,
        });
        if let Some(key) = &signal.dedup_key {
            alert["alias"] = truncate(key, MAX_ALIAS_CHARS).into();
        }
        let description = template::render(&self.templates, "description", signal, &[])
            .unwrap_or_else(|| {
                let mut description = signal.message.clone().unwrap_or_default();
                if let Some(log_url) = &signal.log_url {
                    if !description.is_empty() {
                        description.push_str("\n\n");
                    }
                    description.push_str("Logs: ");
                    description.push_str(log_url);
                }
                description
            });
        if !description.is_empty() {
            alert["description"] = truncate(&description, MAX_DESCRIPTION_CHARS).into();
        }
        let entity = match (&signal.service, &signal.component) {
            (Some(service), Some(component)) => Some(format!("{service}/{component}")),
            (Some(one), None) | (None, Some(one)) => Some(one.clone()),
            (None, None) => None,
        };
        if let Some(entity) = entity {
            alert["entity"] = truncate(&entity, MAX_ENTITY_CHARS).into();
        }
        alert
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
}

#[async_trait::async_trait]
impl Provider for OpsgenieProvider {
    fn name(&self) -> &'static str {
        "opsgenie"
    }

    fn preview(&self, signal: &Signal) -> Option<serde_json::Value> {
        Some(self.alert_for(signal))
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let resp = self
            .client
            .post(&self.url)
            .header("Authorization", format!("GenieKey {}", self.api_key))
            .json(&self.alert_for(signal))
            .send()
            .await
            .map_err(|e| SignalError::Delivery(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SignalError::Delivery(format!(
                "opsgenie alert API returned status {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SignalInput;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn signal(severity: Severity) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            service: Some("orders".into()),
            component: Some("dbt".into()),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            message: Some("model orders timed out".into()),
            severity,
            environment: Some("prod".into()),
            log_url: Some("https://logs.example.test/run/1".into()),
            metadata: Some(serde_json::json!({ "dag": "daily", "try": 2 })),
            ..Default::default()
        })
    }

    #[test]
    fn severities_map_to_opsgenie_priorities() {
        assert_eq!(OpsgenieProvider::priority_for(Severity::Critical), "P1");
        assert_eq!(OpsgenieProvider::priority_for(Severity::Error), "P2");
        assert_eq!(OpsgenieProvider::priority_for(Severity::Warning), "P3");
        assert_eq!(OpsgenieProvider::priority_for(Severity::Info), "P5");
        assert_eq!(OpsgenieProvider::priority_for(Severity::Success), "P5");
    }

    #[test]
    fn defaults_to_the_us_alert_endpoint() {
        let provider = OpsgenieProvider::new("key".into(), None, vec![]);
        assert_eq!(provider.url, DEFAULT_ALERTS_URL);
    }

    #[test]
    fn alert_carries_signal_fields() {
        let provider = OpsgenieProvider::new("k".into(), None, vec!["data".into()]);
        let signal = signal(Severity::Critical);
        let alert = provider.alert_for(&signal);

        assert_eq!(alert["message"], "dbt model failed");
        assert_eq!(alert["alias"], signal.dedup_key.clone().unwrap());
        assert_eq!(alert["source"], "airflow");
        assert_eq!(alert["priority"], "P1");
        assert_eq!(alert["entity"], "orders/dbt");
        assert_eq!(
            alert["description"],
            "model orders timed out\n\nLogs: https://logs.example.test/run/1"
        );
        assert_eq!(
            alert["tags"],
            serde_json::json!(["data", "critical", "prod"])
        );
        let details = &alert["details"];
        assert_eq!(details["signal_id"], signal.id.to_string());
        assert_eq!(details["event_type"], "task_failed");
        assert_eq!(details["environment"], "prod");
        assert_eq!(details["log_url"], "https://logs.example.test/run/1");
        assert_eq!(details["metadata.dag"], "daily");
        // Non-string metadata is flattened to its JSON text.
        assert_eq!(details["metadata.try"], "2");
        assert!(details.get("team").is_none());
    }

    #[test]
    fn minimal_signal_omits_optional_fields() {
        let provider = OpsgenieProvider::new("k".into(), None, vec![]);
        let signal = Signal::from_input(SignalInput {
            source: "ci".into(),
            event_type: "build_failed".into(),
            title: "main is red".into(),
            ..Default::default()
        });
        let alert = provider.alert_for(&signal);
        assert!(alert.get("description").is_none());
        assert!(alert.get("entity").is_none());
        assert_eq!(alert["tags"], serde_json::json!(["info"]));
    }

    #[test]
    fn long_fields_are_cut_to_opsgenie_limits() {
        let tags = (0..30).map(|i| format!("{i}-{}", "x".repeat(60))).collect();
        let provider = OpsgenieProvider::new("k".into(), None, tags);
        let signal = Signal::from_input(SignalInput {
            source: "s".repeat(200),
            event_type: "build_failed".into(),
            // Multi-byte characters, so a byte-based cut would panic.
            title: "é".repeat(500),
            dedup_key: Some("ü".repeat(600)),
            ..Default::default()
        });
        let alert = provider.alert_for(&signal);
        let chars = |v: &serde_json::Value| v.as_str().unwrap().chars().count();
        assert_eq!(chars(&alert["message"]), MAX_MESSAGE_CHARS);
        assert_eq!(chars(&alert["alias"]), MAX_ALIAS_CHARS);
        assert_eq!(chars(&alert["source"]), MAX_SOURCE_CHARS);
        let tags = alert["tags"].as_array().unwrap();
        assert_eq!(tags.len(), MAX_TAGS);
        assert!(tags.iter().all(|t| chars(t) <= MAX_TAG_CHARS));
    }

    #[test]
    fn duplicate_tags_are_dropped() {
        let provider = OpsgenieProvider::new("k".into(), None, vec!["prod".into(), "error".into()]);
        let alert = provider.alert_for(&signal(Severity::Error));
        assert_eq!(alert["tags"], serde_json::json!(["prod", "error"]));
    }

    #[tokio::test]
    async fn posts_the_alert_with_the_genie_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/alerts"))
            .and(header("authorization", "GenieKey S3CRET"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(serde_json::json!({
                "message": "dbt model failed",
                "priority": "P2",
            })))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        OpsgenieProvider::new(
            "S3CRET".into(),
            Some(format!("{}/v2/alerts", server.uri())),
            vec![],
        )
        .send(&signal(Severity::Error))
        .await
        .expect("202 is success");
    }

    #[tokio::test]
    async fn rejected_alert_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = OpsgenieProvider::new("bad".into(), Some(server.uri()), vec![])
            .send(&signal(Severity::Error))
            .await
            .expect_err("401 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(err.to_string().contains("401"));
        // The key must never end up in an error that gets logged.
        assert!(!err.to_string().contains("bad"));
    }

    #[test]
    fn message_and_description_templates_replace_the_defaults() {
        let provider =
            OpsgenieProvider::new("k".into(), None, vec![]).with_templates(template::for_test(
                "opsgenie",
                &[
                    ("message", "{{ service }}: {{ title }}"),
                    (
                        "description",
                        "DAG {{ metadata.dag }}, try {{ metadata.try }}",
                    ),
                ],
            ));
        let alert = provider.alert_for(&signal(Severity::Error));
        assert_eq!(alert["message"], "orders: dbt model failed");
        assert_eq!(alert["description"], "DAG daily, try 2");
    }
}
