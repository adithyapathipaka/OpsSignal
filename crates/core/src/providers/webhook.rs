use crate::delivery::Provider;
use crate::signal::{Signal, SignalError};

pub struct WebhookProvider {
    pub url: String,
    client: reqwest::Client,
}

impl WebhookProvider {
    pub fn new(url: String) -> Self {
        WebhookProvider {
            url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for WebhookProvider {
    fn name(&self) -> &'static str {
        "webhook"
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let resp = self
            .client
            .post(&self.url)
            .json(signal)
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
}
