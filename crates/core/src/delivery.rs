use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    Delivered,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub status: DeliveryStatus,
    pub provider: Option<String>,
    pub error: Option<String>,
    pub attempts: u32,
}

impl DeliveryResult {
    pub fn status_str(&self) -> &'static str {
        match self.status {
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Failed => "failed",
            DeliveryStatus::Skipped => "skipped",
        }
    }
}

/// Every provider (Slack, Webhook, Console, ...) implements this. Adding
/// a provider never requires touching routing or retry logic — that's
/// the abstraction boundary that keeps the core's "don't become an
/// event bus" promise honest.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn send(&self, signal: &crate::signal::Signal) -> Result<(), crate::signal::SignalError>;
}
