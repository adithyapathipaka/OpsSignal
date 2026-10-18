use crate::delivery::Provider;
use crate::signal::{Severity, Signal, SignalError};

/// Events API v2 endpoint for US-hosted accounts. EU accounts use
/// `https://events.eu.pagerduty.com/v2/enqueue`, set through `url`.
pub const DEFAULT_EVENTS_URL: &str = "https://events.pagerduty.com/v2/enqueue";

/// PagerDuty rejects longer values with a 400, which retrying cannot
/// fix, so they are cut to fit instead.
const MAX_SUMMARY_CHARS: usize = 1024;
const MAX_DEDUP_KEY_CHARS: usize = 255;

/// Triggers a PagerDuty alert through the Events API v2. The signal's
/// `dedup_key` becomes PagerDuty's, so a repeat of the same signal adds
/// to the open alert instead of paging again.
pub struct PagerDutyProvider {
    routing_key: String,
    pub url: String,
    client: reqwest::Client,
}

impl PagerDutyProvider {
    pub fn new(routing_key: String, url: Option<String>) -> Self {
        PagerDutyProvider {
            routing_key,
            url: url.unwrap_or_else(|| DEFAULT_EVENTS_URL.to_string()),
            client: reqwest::Client::new(),
        }
    }

    /// PagerDuty has no `success` severity; a success signal is
    /// informational.
    fn severity_for(severity: Severity) -> &'static str {
        match severity {
            Severity::Info | Severity::Success => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Critical => "critical",
        }
    }

    fn event_for(&self, signal: &Signal) -> serde_json::Value {
        let mut details = serde_json::Map::new();
        details.insert("signal_id".into(), signal.id.to_string().into());
        details.insert("event_type".into(), signal.event_type.clone().into());
        details.insert("severity".into(), signal.severity.to_string().into());
        let optional = [
            ("message", &signal.message),
            ("environment", &signal.environment),
            ("team", &signal.team),
            ("run_id", &signal.run_id),
            ("correlation_id", &signal.correlation_id),
            ("trace_id", &signal.trace_id),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                details.insert(key.into(), value.clone().into());
            }
        }
        if signal.metadata.as_object().is_some_and(|m| !m.is_empty()) {
            details.insert("metadata".into(), signal.metadata.clone());
        }

        let mut payload = serde_json::json!({
            "summary": truncate(&signal.title, MAX_SUMMARY_CHARS),
            "source": signal.source,
            "severity": Self::severity_for(signal.severity),
            "timestamp": signal.timestamp.to_rfc3339(),
            "class": signal.event_type,
            "custom_details": details,
        });
        if let Some(component) = &signal.component {
            payload["component"] = component.clone().into();
        }
        if let Some(service) = &signal.service {
            payload["group"] = service.clone().into();
        }

        let mut event = serde_json::json!({
            "routing_key": self.routing_key,
            "event_action": "trigger",
            "payload": payload,
        });
        if let Some(key) = &signal.dedup_key {
            event["dedup_key"] = truncate(key, MAX_DEDUP_KEY_CHARS).into();
        }
        if let Some(log_url) = &signal.log_url {
            event["links"] = serde_json::json!([{ "href": log_url, "text": "Logs" }]);
        }
        event
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
}

#[async_trait::async_trait]
impl Provider for PagerDutyProvider {
    fn name(&self) -> &'static str {
        "pagerduty"
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let resp = self
            .client
            .post(&self.url)
            .json(&self.event_for(signal))
            .send()
            .await
            .map_err(|e| SignalError::Delivery(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SignalError::Delivery(format!(
                "pagerduty events API returned status {}",
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
            metadata: Some(serde_json::json!({ "dag": "daily" })),
            ..Default::default()
        })
    }

    #[test]
    fn severities_map_to_pagerduty_levels() {
        assert_eq!(PagerDutyProvider::severity_for(Severity::Info), "info");
        assert_eq!(PagerDutyProvider::severity_for(Severity::Success), "info");
        assert_eq!(
            PagerDutyProvider::severity_for(Severity::Warning),
            "warning"
        );
        assert_eq!(PagerDutyProvider::severity_for(Severity::Error), "error");
        assert_eq!(
            PagerDutyProvider::severity_for(Severity::Critical),
            "critical"
        );
    }

    #[test]
    fn defaults_to_the_us_events_endpoint() {
        let provider = PagerDutyProvider::new("key".into(), None);
        assert_eq!(provider.url, DEFAULT_EVENTS_URL);
    }

    #[test]
    fn event_carries_signal_fields() {
        let provider = PagerDutyProvider::new("R0UT1NG".into(), None);
        let signal = signal(Severity::Critical);
        let event = provider.event_for(&signal);

        assert_eq!(event["routing_key"], "R0UT1NG");
        assert_eq!(event["event_action"], "trigger");
        assert_eq!(event["dedup_key"], signal.dedup_key.clone().unwrap());
        assert_eq!(event["links"][0]["href"], "https://logs.example.test/run/1");
        let payload = &event["payload"];
        assert_eq!(payload["summary"], "dbt model failed");
        assert_eq!(payload["source"], "airflow");
        assert_eq!(payload["severity"], "critical");
        assert_eq!(payload["component"], "dbt");
        assert_eq!(payload["group"], "orders");
        assert_eq!(payload["class"], "task_failed");
        assert_eq!(payload["timestamp"], signal.timestamp.to_rfc3339());
        let details = &payload["custom_details"];
        assert_eq!(details["signal_id"], signal.id.to_string());
        assert_eq!(details["message"], "model orders timed out");
        assert_eq!(details["environment"], "prod");
        assert_eq!(details["metadata"]["dag"], "daily");
        assert!(details.get("team").is_none());
    }

    #[test]
    fn minimal_signal_omits_optional_fields() {
        let provider = PagerDutyProvider::new("k".into(), None);
        let signal = Signal::from_input(SignalInput {
            source: "ci".into(),
            event_type: "build_failed".into(),
            title: "main is red".into(),
            ..Default::default()
        });
        let event = provider.event_for(&signal);
        assert!(event.get("links").is_none());
        assert!(event["payload"].get("component").is_none());
        assert!(event["payload"].get("group").is_none());
        assert!(event["payload"]["custom_details"].get("metadata").is_none());
    }

    #[test]
    fn long_summary_and_dedup_key_are_cut_to_pagerduty_limits() {
        let provider = PagerDutyProvider::new("k".into(), None);
        let signal = Signal::from_input(SignalInput {
            source: "ci".into(),
            event_type: "build_failed".into(),
            // Multi-byte characters, so a byte-based cut would panic.
            title: "é".repeat(2000),
            dedup_key: Some("ü".repeat(300)),
            ..Default::default()
        });
        let event = provider.event_for(&signal);
        let summary = event["payload"]["summary"].as_str().unwrap();
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);
        let key = event["dedup_key"].as_str().unwrap();
        assert_eq!(key.chars().count(), MAX_DEDUP_KEY_CHARS);
    }

    #[tokio::test]
    async fn posts_the_event_to_the_events_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/enqueue"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(serde_json::json!({
                "routing_key": "R0UT1NG",
                "event_action": "trigger",
                "payload": { "summary": "dbt model failed", "severity": "error" },
            })))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        PagerDutyProvider::new(
            "R0UT1NG".into(),
            Some(format!("{}/v2/enqueue", server.uri())),
        )
        .send(&signal(Severity::Error))
        .await
        .expect("202 is success");
    }

    #[tokio::test]
    async fn rejected_event_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let err = PagerDutyProvider::new("bad".into(), Some(server.uri()))
            .send(&signal(Severity::Error))
            .await
            .expect_err("400 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn rate_limit_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let err = PagerDutyProvider::new("k".into(), Some(server.uri()))
            .send(&signal(Severity::Error))
            .await
            .expect_err("429 must fail so the retry loop backs off");
        assert!(err.to_string().contains("429"));
    }
}
