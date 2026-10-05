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
