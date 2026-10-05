#![forbid(unsafe_code)]

pub mod config;
pub mod delivery;
pub mod providers;
pub mod routing;
pub mod signal;
pub mod sink;

const MAX_RETRY_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 200;

use config::{Config, SinkKind};
use delivery::{DeliveryResult, DeliveryStatus, Provider};
use providers::{console::ConsoleProvider, slack::SlackProvider, webhook::WebhookProvider};
use routing::RoutingConfig;
use signal::{Signal, SignalError, SignalInput};
use sink::{SignalSink, SqliteSink};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The assembled pipeline: validate -> persist + route happen off the
/// same validated signal, in parallel conceptually, so a sink failure
/// can never block delivery and vice versa (see architecture review,
/// component diagram).
pub struct SignalClient {
    pub sink: Arc<dyn SignalSink>,
    pub routing: RoutingConfig,
    pub providers: HashMap<String, Arc<dyn Provider>>,
    pub runtime: tokio::runtime::Runtime,
}

impl SignalClient {
    /// Builds a client from a parsed `signal.yaml`: registers the
    /// configured providers (console always), opens the sink, and
    /// starts the runtime that delivery tasks run on.
    pub fn from_config(config: &Config) -> Result<Self, SignalError> {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("console".to_string(), Arc::new(ConsoleProvider));
        if let Some(slack) = &config.providers.slack {
            providers.insert(
                "slack".to_string(),
                Arc::new(SlackProvider::new(slack.webhook_url.clone())),
            );
        }
        if let Some(webhook) = &config.providers.webhook {
            providers.insert(
                "webhook".to_string(),
                Arc::new(WebhookProvider::new(webhook.url.clone())),
            );
        }

        let sink: Arc<dyn SignalSink> = match config.storage.sink {
            SinkKind::Sqlite => Arc::new(SqliteSink::open(&config.storage.path)?),
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("opssignal")
            .enable_all()
            .build()
            .map_err(|e| SignalError::Internal(format!("failed to start runtime: {e}")))?;

        Ok(SignalClient {
            sink,
            routing: config.routing(),
            providers,
            runtime,
        })
    }

    /// `from_config` on the discovered config: `$SIGNAL_CONFIG`, then
    /// `./signal.yaml`, then the zero-config default.
    pub fn from_env() -> Result<Self, SignalError> {
        Self::from_config(&Config::discover()?)
    }

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
    #[must_use = "check whether the signal was delivered"]
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
    providers: &HashMap<String, Arc<dyn Provider>>,
) -> DeliveryResult {
    let provider_names = routing.resolve(signal);
    let mut last_error = None;

    for name in &provider_names {
        if let Some(provider) = providers.get(name) {
            // Basic retry: exponential backoff up to MAX_RETRY_ATTEMPTS.
            // v0.2 roadmap item: add jitter, make configurable per-route.
            for attempt in 1..=MAX_RETRY_ATTEMPTS {
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
                        if attempt < MAX_RETRY_ATTEMPTS {
                            tokio::time::sleep(Duration::from_millis(
                                INITIAL_BACKOFF_MS * 2u64.pow(attempt),
                            ))
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
        attempts: MAX_RETRY_ATTEMPTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Severity;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn temp_db() -> String {
        std::env::temp_dir()
            .join(format!("opssignal-test-{}.db", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned()
    }

    fn input(severity: Severity) -> SignalInput {
        SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            severity,
            environment: Some("prod".into()),
            ..Default::default()
        }
    }

    fn stored_rows(db: &str) -> Vec<(String, Option<String>)> {
        let conn = rusqlite::Connection::open(db).expect("open db");
        let mut stmt = conn
            .prepare("SELECT delivery_status, provider FROM signals")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    }

    #[test]
    fn zero_config_client_delivers_to_console_and_records_it() {
        let db = temp_db();
        let yaml = format!("storage:\n  path: \"{db}\"\n");
        let client = SignalClient::from_config(&Config::from_yaml_str(&yaml).expect("config"))
            .expect("client");

        let result = client
            .notify_sync(input(Severity::Info), Duration::from_secs(5))
            .expect("notify_sync");

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("console"));
        assert_eq!(
            stored_rows(&db),
            vec![("delivered".to_string(), Some("console".to_string()))]
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn configured_route_delivers_to_webhook() {
        // The mock server lives on its own runtime; notify_sync blocks
        // on the client's runtime, so the two must not be nested.
        let server_rt = tokio::runtime::Runtime::new().expect("server runtime");
        let server = server_rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            server
        });

        let db = temp_db();
        let yaml = format!(
            "providers:\n  webhook:\n    url: \"{}/hook\"\nroutes:\n  - match:\n      severity: [error, critical]\n      environment: prod\n    providers: [webhook]\nstorage:\n  path: \"{db}\"\n",
            server.uri()
        );
        let client = SignalClient::from_config(&Config::from_yaml_str(&yaml).expect("config"))
            .expect("client");

        let result = client
            .notify_sync(input(Severity::Error), Duration::from_secs(5))
            .expect("notify_sync");

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("webhook"));
        server_rt.block_on(server.verify());
        let _ = std::fs::remove_file(&db);
    }
}
