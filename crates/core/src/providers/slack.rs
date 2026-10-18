use crate::delivery::Provider;
use crate::signal::{Severity, Signal, SignalError};

pub struct SlackProvider {
    pub webhook_url: String,
    client: reqwest::Client,
}

impl SlackProvider {
    pub fn new(webhook_url: String) -> Self {
        SlackProvider {
            webhook_url,
            client: reqwest::Client::new(),
        }
    }

    fn emoji_for(severity: Severity) -> &'static str {
        match severity {
            Severity::Info => ":information_source:",
            Severity::Success => ":white_check_mark:",
            Severity::Warning => ":warning:",
            Severity::Error => ":x:",
            Severity::Critical => ":rotating_light:",
        }
    }
}

#[async_trait::async_trait]
impl Provider for SlackProvider {
    fn name(&self) -> &'static str {
        "slack"
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let text = format!(
            "{} *{}*\n{}",
            Self::emoji_for(signal.severity),
            signal.title,
            signal.message.clone().unwrap_or_default()
        );
        let body = serde_json::json!({ "text": text });

        let resp = self
            .client
            .post(&self.webhook_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SignalError::Delivery(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SignalError::Delivery(format!(
                "slack webhook returned status {}",
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
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn signal(severity: Severity, message: Option<&str>) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            message: message.map(str::to_string),
            severity,
            ..Default::default()
        })
    }

    #[test]
    fn every_severity_has_a_distinct_emoji() {
        let all = [
            Severity::Info,
            Severity::Success,
            Severity::Warning,
            Severity::Error,
            Severity::Critical,
        ];
        let mut emojis: Vec<&str> = all.iter().map(|s| SlackProvider::emoji_for(*s)).collect();
        emojis.sort_unstable();
        emojis.dedup();
        assert_eq!(emojis.len(), all.len());
        assert_eq!(
            SlackProvider::emoji_for(Severity::Critical),
            ":rotating_light:"
        );
    }

    #[tokio::test]
    async fn posts_slack_formatted_text_to_the_webhook() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/T/B/X"))
            .and(header("content-type", "application/json"))
            .and(body_json(serde_json::json!({
                "text": ":x: *dbt model failed*\nmodel orders timed out"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let provider = SlackProvider::new(format!("{}/services/T/B/X", server.uri()));
        provider
            .send(&signal(Severity::Error, Some("model orders timed out")))
            .await
            .expect("delivered");
    }

    #[tokio::test]
    async fn missing_message_sends_title_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "text": ":information_source: *dbt model failed*\n"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        SlackProvider::new(server.uri())
            .send(&signal(Severity::Info, None))
            .await
            .expect("delivered");
    }

    #[tokio::test]
    async fn non_success_status_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = SlackProvider::new(server.uri())
            .send(&signal(Severity::Error, None))
            .await
            .expect_err("500 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn unreachable_webhook_is_a_delivery_error() {
        // Bind then drop a listener so the port is known to be closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();

        let err = SlackProvider::new(format!("http://127.0.0.1:{port}/hook"))
            .send(&signal(Severity::Error, None))
            .await
            .expect_err("connection refused must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
    }
}
