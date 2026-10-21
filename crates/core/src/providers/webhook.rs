use crate::delivery::Provider;
use crate::signal::{Signal, SignalError};
use crate::template::{self, Templates};

pub struct WebhookProvider {
    pub url: String,
    client: reqwest::Client,
    templates: Option<Templates>,
}

impl WebhookProvider {
    pub fn new(url: String) -> Self {
        WebhookProvider {
            url,
            client: reqwest::Client::new(),
            templates: None,
        }
    }

    /// Uses `templates` for the parts they set; see [`crate::template`].
    pub fn with_templates(mut self, templates: Option<Templates>) -> Self {
        self.templates = templates;
        self
    }

    /// The templated `body` when it renders to valid JSON, else the
    /// signal itself.
    fn body_for(&self, signal: &Signal) -> serde_json::Value {
        if let Some(text) = template::render(&self.templates, "body", signal, &[]) {
            match serde_json::from_str(&text) {
                Ok(body) => return body,
                Err(e) => tracing::warn!(
                    signal_id = %signal.id,
                    error = %e,
                    "webhook body template is not valid JSON; sending the signal instead"
                ),
            }
        }
        serde_json::to_value(signal).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Provider for WebhookProvider {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn preview(&self, signal: &Signal) -> Option<serde_json::Value> {
        Some(self.body_for(signal))
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let resp = self
            .client
            .post(&self.url)
            .json(&self.body_for(signal))
            .send()
            .await
            .map_err(|e| SignalError::Delivery(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SignalError::Delivery(format!(
                "webhook returned status {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Severity, SignalInput};
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn signal() -> Signal {
        Signal::from_input(SignalInput {
            source: "ci".into(),
            event_type: "build_failed".into(),
            title: "main is red".into(),
            severity: Severity::Critical,
            environment: Some("prod".into()),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn posts_the_full_signal_as_json() {
        let server = MockServer::start().await;
        let signal = signal();
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(body_partial_json(serde_json::json!({
                "id": signal.id.to_string(),
                "source": "ci",
                "event_type": "build_failed",
                "severity": "critical",
                "title": "main is red",
                "environment": "prod",
                "dedup_key": signal.dedup_key,
            })))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        WebhookProvider::new(format!("{}/hook", server.uri()))
            .send(&signal)
            .await
            .expect("any 2xx is success");
    }

    #[tokio::test]
    async fn non_success_status_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = WebhookProvider::new(server.uri())
            .send(&signal())
            .await
            .expect_err("404 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn invalid_url_is_a_delivery_error() {
        let err = WebhookProvider::new("not a url".into())
            .send(&signal())
            .await
            .expect_err("invalid URL must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
    }

    #[tokio::test]
    async fn body_template_sends_custom_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "alert": "main is red",
                "level": "CRITICAL",
                "labels": { "env": "prod" },
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        WebhookProvider::new(server.uri())
            .with_templates(template::for_test(
                "webhook",
                &[(
                    "body",
                    r#"{"alert": {{ title|tojson }}, "level": {{ severity|upper|tojson }}, "labels": {"env": {{ environment|tojson }}}}"#,
                )],
            ))
            .send(&signal())
            .await
            .expect("delivered");
    }

    #[tokio::test]
    async fn body_that_is_not_json_sends_the_signal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                serde_json::json!({ "title": "main is red" }),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        WebhookProvider::new(server.uri())
            .with_templates(template::for_test(
                "webhook",
                &[("body", "alert: {{ title }}")],
            ))
            .send(&signal())
            .await
            .expect("delivered");
    }
}
