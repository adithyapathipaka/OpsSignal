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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SignalInput;

    #[tokio::test]
    async fn console_always_succeeds() {
        let signal = Signal::from_input(SignalInput {
            source: "x".into(),
            event_type: "y".into(),
            title: "z".into(),
            ..Default::default()
        });
        assert_eq!(ConsoleProvider.name(), "console");
        ConsoleProvider
            .send(&signal)
            .await
            .expect("console never fails");
    }
}
