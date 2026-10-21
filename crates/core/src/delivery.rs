use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    /// Recorded, delivery not finished yet. A row left in this state
    /// means the process exited before delivery completed.
    Pending,
    Delivered,
    /// A fan-out reached some of its targets but not all of them.
    Partial,
    Failed,
    /// Not sent: every route it matched was held back by its `dedup` or
    /// `rate_limit`.
    Skipped,
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DeliveryStatus::Pending => "pending",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Partial => "partial",
            DeliveryStatus::Failed => "failed",
            DeliveryStatus::Skipped => "skipped",
        })
    }
}

/// The overall outcome of delivering one signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub status: DeliveryStatus,
    /// The providers that delivered, comma separated in plan order
    /// (`slack,pagerduty`), or `None` if none did.
    pub provider: Option<String>,
    /// Why delivery failed. With more than one target, each failed
    /// target's last error, prefixed with its provider.
    pub error: Option<String>,
    /// Attempts made on the provider that decided the outcome; the most
    /// any target needed when there are several. `deliveries` has the
    /// count for every provider.
    pub attempts: u32,
    /// One entry per provider tried, in plan order.
    #[serde(default)]
    pub deliveries: Vec<ProviderOutcome>,
    /// Matching routes held back by their `dedup` or `rate_limit`, one
    /// line per route with the reason. A signal can be delivered on one
    /// route and suppressed on another.
    #[serde(default)]
    pub suppressed: Vec<String>,
}

impl DeliveryResult {
    /// What the sink stores before delivery starts.
    pub fn pending() -> Self {
        DeliveryResult {
            status: DeliveryStatus::Pending,
            provider: None,
            error: None,
            attempts: 0,
            deliveries: Vec::new(),
            suppressed: Vec::new(),
        }
    }
}

/// How delivery to one provider went: `pending` while it runs, then
/// `delivered` or `failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOutcome {
    pub provider: String,
    pub status: DeliveryStatus,
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
