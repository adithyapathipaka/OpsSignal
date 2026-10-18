use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    /// Recorded, delivery not finished yet. A row left in this state
    /// means the process exited before delivery completed.
    Pending,
    Delivered,
    Failed,
    Skipped,
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DeliveryStatus::Pending => "pending",
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

impl DeliveryResult {
    /// What the sink stores before delivery starts.
    pub fn pending() -> Self {
        DeliveryResult {
            status: DeliveryStatus::Pending,
            provider: None,
            error: None,
            attempts: 0,
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
