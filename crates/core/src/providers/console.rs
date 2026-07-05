use crate::delivery::Provider;
use crate::signal::{Signal, SignalError};

/// Always-available fallback. Per the architecture review: unmatched
/// signals must go SOMEWHERE — a notification SDK that silently drops
/// signals is worse than having no SDK at all. Console is the
/// guaranteed-to-work last resort.
pub struct ConsoleProvider;

#[async_trait::async_trait]
impl Provider for ConsoleProvider {
    fn name(&self) -> &'static str {
        "console"
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        println!(
            "[signal:{}] {} {} — {}",
            signal.source, signal.severity, signal.event_type, signal.title
        );
        Ok(())
    }
}
