pub mod delivery;
pub mod providers;
pub mod routing;
pub mod signal;
pub mod sink;

use delivery::{DeliveryResult, DeliveryStatus, Provider};
use routing::RoutingConfig;
use signal::{Signal, SignalError, SignalInput};
use sink::SignalSink;
use std::sync::Arc;
use std::time::Duration;

/// The assembled pipeline: validate -> persist + route happen off the
/// same validated signal, in parallel conceptually, so a sink failure
/// can never block delivery and vice versa (see architecture review,
/// component diagram).
pub struct SignalClient {
    pub sink: Arc<dyn SignalSink>,
    pub routing: RoutingConfig,
    pub providers: std::collections::HashMap<String, Arc<dyn Provider>>,
    pub runtime: tokio::runtime::Runtime,
}

impl SignalClient {
    /// Fire-and-forget. Never blocks the caller beyond spawning the
    /// task. This is the default an Airflow on_failure_callback should
    /// use — a flaky Slack webhook must never add latency or failure
    /// risk to the DAG run itself.
    pub fn notify_async(&self, input: SignalInput) {
        if let Err(e) = signal::validate(&input) {
            tracing::warn!("signal validation failed, dropping: {e}");
            return;
        }
        let signal = Signal::from_input(input);
        let sink = Arc::clone(&self.sink);
        let routing = self.routing.clone();
        let providers = self.providers.clone();

        self.runtime.spawn(async move {
            let result = deliver(&signal, &routing, &providers).await;
            if let Err(e) = sink.write(&signal, &result) {
                tracing::error!("sink write failed: {e}");
            }
        });
    }

    /// Blocking, bounded by timeout. Caller gets a definitive result.
    /// Same pipeline as notify_async — only the blocking behavior
    /// differs.
    pub fn notify_sync(
        &self,
        input: SignalInput,
        timeout: Duration,
    ) -> Result<DeliveryResult, SignalError> {
        signal::validate(&input)?;
        let signal = Signal::from_input(input);
        let routing = self.routing.clone();
        let providers = self.providers.clone();

        let result = self.runtime.block_on(async {
            tokio::time::timeout(timeout, deliver(&signal, &routing, &providers))
                .await
                .unwrap_or_else(|_| DeliveryResult {
                    status: DeliveryStatus::Failed,
                    provider: None,
                    error: Some(format!("timed out after {timeout:?}")),
                    attempts: 0,
                })
        });

        self.sink.write(&signal, &result)?;
        Ok(result)
    }
}

async fn deliver(
    signal: &Signal,
    routing: &RoutingConfig,
    providers: &std::collections::HashMap<String, Arc<dyn Provider>>,
) -> DeliveryResult {
    let provider_names = routing.resolve(signal);
    let mut last_error = None;

    for name in &provider_names {
        if let Some(provider) = providers.get(name) {
            // Basic retry: 3 attempts, exponential backoff.
            // v0.2 roadmap item: tune this, add jitter, make configurable.
            for attempt in 1..=3u32 {
                match provider.send(signal).await {
                    Ok(()) => {
                        return DeliveryResult {
                            status: DeliveryStatus::Delivered,
                            provider: Some(provider.name().to_string()),
                            error: None,
                            attempts: attempt,
                        };
                    }
                    Err(e) => {
                        last_error = Some(e.to_string());
                        if attempt < 3 {
                            tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt)))
                                .await;
                        }
                    }
                }
            }
        } else {
            tracing::warn!("route references unknown provider: {name}");
        }
    }

    DeliveryResult {
        status: DeliveryStatus::Failed,
        provider: None,
        error: last_error.or_else(|| Some("no provider delivered successfully".to_string())),
        attempts: 3,
    }
}
