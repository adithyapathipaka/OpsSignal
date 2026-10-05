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
