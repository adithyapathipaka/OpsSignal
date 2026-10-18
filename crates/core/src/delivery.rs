use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    Delivered,
    Failed,
    Skipped,
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Failed => "failed",
            DeliveryStatus::Skipped => "skipped",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub status: DeliveryStatus,
    pub provider: Option<String>,
    pub error: Option<String>,
    pub attempts: u32,
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
