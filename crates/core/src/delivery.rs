use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Held for a digest by a route's `group_by` or `group_wait`, and
    /// matched no route that sends now. The stored row moves to the
    /// digest's outcome once the digest is sent.
    Grouped,
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DeliveryStatus::Pending => "pending",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Partial => "partial",
            DeliveryStatus::Failed => "failed",
            DeliveryStatus::Skipped => "skipped",
            DeliveryStatus::Grouped => "grouped",
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
    /// Matching routes holding the signal for a digest, one line per
    /// route. A signal can be sent now on one route and held on another.
    #[serde(default)]
    pub grouped: Vec<String>,
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
            grouped: Vec::new(),
        }
    }
}

/// What a blocking send returns: which signal was sent and how its
/// delivery went. Serializes as one flat JSON object, the shape the C
/// library's `signal_send` hands back.
#[derive(Debug, Clone, Serialize)]
pub struct SendReport {
    /// The core-generated id the signal was stored under.
    pub signal_id: uuid::Uuid,
    /// The incident key: the caller's, or the one computed from the
    /// signal. Keep it to resolve or deduplicate the incident later.
    pub dedup_key: Option<String>,
    #[serde(flatten)]
    pub result: DeliveryResult,
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

    /// What the core calls to deliver. `alert_ref` is what this provider
    /// returned when it delivered the alert an `acknowledge` or
    /// `resolve` acts on, such as the Slack message to update; a
    /// provider that needs no reference returns `None` and leaves this
    /// as is, implementing only `send`.
    async fn send_with_ref(
        &self,
        signal: &crate::signal::Signal,
        _alert_ref: Option<&str>,
    ) -> Result<Option<String>, crate::signal::SignalError> {
        self.send(signal).await.map(|()| None)
    }

    /// Whether the provider acknowledges and resolves alerts by dedup
    /// key on its own side (PagerDuty, Opsgenie), so an `acknowledge` or
    /// `resolve` can go to it even when this process has no record of
    /// the alert, as when another host raised it.
    fn resolves_by_key(&self) -> bool {
        false
    }

    /// What `send` would deliver for `signal`, without sending it, for
    /// checking templates (`signal preview`). `None` when the provider
    /// can't show it.
    fn preview(&self, _signal: &crate::signal::Signal) -> Option<serde_json::Value> {
        None
    }
}
