#![forbid(unsafe_code)]

mod chain;
#[cfg(feature = "object-store")]
mod cloud;
pub mod config;
pub mod delivery;
mod inflight;
pub mod providers;
mod reload;
pub mod routing;
pub mod secrets;
pub mod signal;
pub mod sink;
pub mod source;

const MAX_RETRY_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 200;

/// How long dropping a [`SignalClient`] waits for fire-and-forget
/// deliveries still in flight.
pub const DEFAULT_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

use config::{Config, SinkKind};
use delivery::{DeliveryResult, DeliveryStatus, Provider};
use inflight::InFlight;
use providers::{
    console::ConsoleProvider, opsgenie::OpsgenieProvider, pagerduty::PagerDutyProvider,
    slack::SlackProvider, teams::TeamsProvider, webhook::WebhookProvider,
};
use routing::RoutingConfig;
use signal::{Signal, SignalError, SignalInput};
use sink::{SignalSink, SqliteSink};
use source::{ConfigSource, LoadOptions};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The part of a client that a config reload replaces: the routing
/// rules and the providers they name. Swapped as one unit, so a signal
/// never sees new routes with old providers.
pub struct Pipeline {
    pub routing: RoutingConfig,
    pub providers: HashMap<String, Arc<dyn Provider>>,
}

impl Pipeline {
    /// Registers the configured providers. `console` is always present
    /// because routing falls back to it.
    pub fn from_config(config: &Config) -> Self {
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
        if let Some(pagerduty) = &config.providers.pagerduty {
            providers.insert(
                "pagerduty".to_string(),
                Arc::new(PagerDutyProvider::new(
                    pagerduty.routing_key.clone(),
                    pagerduty.url.clone(),
                )),
            );
        }
        if let Some(teams) = &config.providers.teams {
            providers.insert(
                "teams".to_string(),
                Arc::new(TeamsProvider::new(teams.webhook_url.clone())),
            );
        }
        if let Some(opsgenie) = &config.providers.opsgenie {
            providers.insert(
                "opsgenie".to_string(),
                Arc::new(OpsgenieProvider::new(
                    opsgenie.api_key.clone(),
                    opsgenie.url.clone(),
                    opsgenie.tags.clone(),
                )),
            );
        }
        Pipeline {
            routing: config.routing(),
            providers,
        }
    }
}

type SharedPipeline = Arc<RwLock<Arc<Pipeline>>>;

/// The assembled pipeline: validate -> record -> route -> deliver ->
/// complete. The signal is saved before delivery starts, so it survives
/// a process exit mid-delivery as `pending`. A sink failure is logged
/// and never stops delivery.
///
/// Dropping the client waits up to [`DEFAULT_FLUSH_TIMEOUT`] for
/// fire-and-forget deliveries; call [`SignalClient::flush`] to choose
/// the wait, or when the client lives in a static that is never dropped.
pub struct SignalClient {
    pub sink: Arc<dyn SignalSink>,
    pipeline: SharedPipeline,
    in_flight: Arc<InFlight>,
    pub runtime: tokio::runtime::Runtime,
}

impl SignalClient {
    /// Builds a client from a parsed `signal.yaml`: registers the
    /// configured providers (console always), opens the sink, and
    /// starts the runtime that delivery tasks run on. The config is
    /// fixed for the client's lifetime; see [`SignalClient::from_source`]
    /// for one that reloads.
    pub fn from_config(config: &Config) -> Result<Self, SignalError> {
        Self::with_runtime(config, build_runtime()?)
    }

    /// Loads the config from `source` and, if `opts.refresh` is set,
    /// keeps re-reading it in the background and applies changes
    /// without a restart. A reload that fails to fetch or parse keeps
    /// the current config, so a bad edit can't take alerting down.
    pub fn from_source(source: &ConfigSource, opts: &LoadOptions) -> Result<Self, SignalError> {
        Self::from_sources(std::slice::from_ref(source), opts)
    }

    /// [`SignalClient::from_source`] for a layered config: `sources`
    /// are merged lowest priority first, along with every file they
    /// `extends`, and any of them changing triggers a reload.
    pub fn from_sources(sources: &[ConfigSource], opts: &LoadOptions) -> Result<Self, SignalError> {
        let runtime = build_runtime()?;
        let loaded = runtime.block_on(source::load_initial(sources, opts))?;
        let client = Self::with_runtime(&loaded.config, runtime)?;
        if let Some(every) = opts.refresh {
            reload::spawn(
                &client.runtime,
                reload::Reloader {
                    roots: sources.to_vec(),
                    opts: opts.clone(),
                    every,
                    last_texts: loaded.texts,
                    storage: loaded.config.storage.clone(),
                    pipeline: Arc::clone(&client.pipeline),
                },
            );
        }
        Ok(client)
    }

    /// The discovered config (`$SIGNAL_CONFIG`, then `./signal.yaml`,
    /// then the zero-config default), reloaded on the interval in
    /// `$SIGNAL_CONFIG_REFRESH_SECS`.
    pub fn from_env() -> Result<Self, SignalError> {
        let opts = LoadOptions::from_env()?;
        match ConfigSource::discover()? {
            Some(sources) => Self::from_sources(&sources, &opts),
            None => Self::from_config(&Config::default()),
        }
    }

    fn with_runtime(
        config: &Config,
        runtime: tokio::runtime::Runtime,
    ) -> Result<Self, SignalError> {
        let sink: Arc<dyn SignalSink> = match config.storage.sink {
            SinkKind::Sqlite => Arc::new(SqliteSink::open(&config.storage.path)?),
        };
        Ok(SignalClient {
            sink,
            pipeline: Arc::new(RwLock::new(Arc::new(Pipeline::from_config(config)))),
            in_flight: Arc::default(),
            runtime,
        })
    }

    /// The routing rules and providers currently in effect.
    pub fn pipeline(&self) -> Arc<Pipeline> {
        current(&self.pipeline)
    }

    /// Fire-and-forget. Blocks the caller only for the local sink
    /// write, never on a provider. This is the default an Airflow
    /// on_failure_callback should use — a flaky Slack webhook must never
    /// add latency or failure risk to the DAG run itself.
    pub fn notify_async(&self, input: SignalInput) {
        if let Err(e) = signal::validate(&input) {
            tracing::warn!("signal validation failed, dropping: {e}");
            return;
        }
        let signal = Signal::from_input(input);
        // On the caller's thread, so the signal is on disk before this
        // returns even if the process exits right after.
        record(self.sink.as_ref(), &signal);
        let sink = Arc::clone(&self.sink);
        let pipeline = self.pipeline();
        let guard = self.in_flight.start();

        self.runtime.spawn(async move {
            let _guard = guard;
            let result = deliver(&signal, &pipeline.routing, &pipeline.providers).await;
            if let Err(e) = sink.complete(&signal, &result) {
                tracing::error!("sink write failed: {e}");
            }
        });
    }

    /// Waits up to `timeout` for every [`SignalClient::notify_async`]
    /// delivery started so far to finish. Returns false if some were
    /// still running; those stay recorded as `pending`. Call it before
    /// the process exits.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.in_flight.wait(timeout) == 0
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
        record(self.sink.as_ref(), &signal);
        let pipeline = self.pipeline();

        let result = self.runtime.block_on(async {
            tokio::time::timeout(
                timeout,
                deliver(&signal, &pipeline.routing, &pipeline.providers),
            )
            .await
            .unwrap_or_else(|_| DeliveryResult {
                status: DeliveryStatus::Failed,
                provider: None,
                error: Some(format!("timed out after {timeout:?}")),
                attempts: 0,
            })
        });

        self.sink.complete(&signal, &result)?;
        Ok(result)
    }
}

impl Drop for SignalClient {
    fn drop(&mut self) {
        if !self.flush(DEFAULT_FLUSH_TIMEOUT) {
            tracing::warn!(
                "exiting with signals still being delivered; they stay recorded as pending"
            );
        }
    }
}

/// A sink that can't record must not stop the alert going out;
/// `complete` writes the whole row again afterwards.
fn record(sink: &dyn SignalSink, signal: &Signal) {
    if let Err(e) = sink.record(signal) {
        tracing::error!("sink write failed, delivering anyway: {e}");
    }
}

fn build_runtime() -> Result<tokio::runtime::Runtime, SignalError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("opssignal")
        .enable_all()
        .build()
        .map_err(|e| SignalError::Internal(format!("failed to start runtime: {e}")))
}

/// A poisoned lock only means a reload panicked mid-swap; the value
/// inside is still a complete pipeline, so keep using it.
fn current(pipeline: &SharedPipeline) -> Arc<Pipeline> {
    Arc::clone(&pipeline.read().unwrap_or_else(|e| e.into_inner()))
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
    use wiremock::matchers::{method, path};
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

    /// A provider that fails its first `fail_times` sends, then succeeds,
    /// counting every call so tests can assert on retry behaviour.
    struct FlakyProvider {
        name: &'static str,
        fail_times: u32,
        calls: std::sync::atomic::AtomicU32,
    }

    impl FlakyProvider {
        fn new(name: &'static str, fail_times: u32) -> Arc<Self> {
            Arc::new(FlakyProvider {
                name,
                fail_times,
                calls: std::sync::atomic::AtomicU32::new(0),
            })
        }

        fn calls(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn send(&self, _signal: &Signal) -> Result<(), SignalError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if call <= self.fail_times {
                Err(SignalError::Delivery(format!(
                    "{} failure #{call}",
                    self.name
                )))
            } else {
                Ok(())
            }
        }
    }

    /// Never finishes within any reasonable timeout.
    struct HangingProvider;

    #[async_trait::async_trait]
    impl Provider for HangingProvider {
        fn name(&self) -> &'static str {
            "hanging"
        }

        async fn send(&self, _signal: &Signal) -> Result<(), SignalError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(())
        }
    }

    fn routing_to(providers: &[&str]) -> RoutingConfig {
        RoutingConfig {
            routes: vec![routing::Route {
                match_rule: routing::RouteMatch {
                    severity: vec![],
                    environment: None,
                    source: None,
                },
                providers: providers.iter().map(|p| p.to_string()).collect(),
            }],
        }
    }

    fn registry(providers: Vec<Arc<dyn Provider>>) -> HashMap<String, Arc<dyn Provider>> {
        providers
            .into_iter()
            .map(|p| (p.name().to_string(), p))
            .collect()
    }

    fn signal() -> Signal {
        Signal::from_input(input(Severity::Error))
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_succeeds_first_try_without_backoff() {
        let ok = FlakyProvider::new("ok", 0);
        let start = tokio::time::Instant::now();

        let result = deliver(&signal(), &routing_to(&["ok"]), &registry(vec![ok.clone()])).await;

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("ok"));
        assert_eq!(result.attempts, 1);
        assert_eq!(ok.calls(), 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_retries_with_exponential_backoff_then_succeeds() {
        let flaky = FlakyProvider::new("flaky", 2);
        let start = tokio::time::Instant::now();

        let result = deliver(
            &signal(),
            &routing_to(&["flaky"]),
            &registry(vec![flaky.clone()]),
        )
        .await;

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.attempts, 3);
        assert_eq!(flaky.calls(), 3);
        // Backoff after attempt n is INITIAL_BACKOFF_MS * 2^n: 400ms + 800ms.
        assert_eq!(start.elapsed(), Duration::from_millis(1200));
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_falls_back_to_next_provider_after_retries_are_exhausted() {
        let broken = FlakyProvider::new("broken", u32::MAX);
        let backup = FlakyProvider::new("backup", 0);

        let result = deliver(
            &signal(),
            &routing_to(&["broken", "backup"]),
            &registry(vec![broken.clone(), backup.clone()]),
        )
        .await;

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("backup"));
        assert_eq!(result.attempts, 1);
        assert_eq!(broken.calls(), MAX_RETRY_ATTEMPTS);
        assert_eq!(backup.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_stops_at_first_successful_provider() {
        let first = FlakyProvider::new("first", 0);
        let second = FlakyProvider::new("second", 0);

        let result = deliver(
            &signal(),
            &routing_to(&["first", "second"]),
            &registry(vec![first.clone(), second.clone()]),
        )
        .await;

        assert_eq!(result.provider.as_deref(), Some("first"));
        assert_eq!(second.calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_reports_last_error_when_every_provider_fails() {
        let a = FlakyProvider::new("a", u32::MAX);
        let b = FlakyProvider::new("b", u32::MAX);

        let result = deliver(
            &signal(),
            &routing_to(&["a", "b"]),
            &registry(vec![a.clone(), b.clone()]),
        )
        .await;

        assert_eq!(result.status, DeliveryStatus::Failed);
        assert_eq!(result.provider, None);
        assert_eq!(result.attempts, MAX_RETRY_ATTEMPTS);
        assert_eq!(
            result.error.as_deref(),
            Some("delivery failed: b failure #3")
        );
        assert_eq!(a.calls(), MAX_RETRY_ATTEMPTS);
        assert_eq!(b.calls(), MAX_RETRY_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_skips_unregistered_provider_and_uses_the_next() {
        let ok = FlakyProvider::new("ok", 0);

        let result = deliver(
            &signal(),
            &routing_to(&["missing", "ok"]),
            &registry(vec![ok.clone()]),
        )
        .await;

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("ok"));
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_with_no_registered_provider_fails_with_a_message() {
        let result = deliver(&signal(), &routing_to(&["missing"]), &HashMap::new()).await;

        assert_eq!(result.status, DeliveryStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some("no provider delivered successfully")
        );
    }

    fn client_with(providers: Vec<Arc<dyn Provider>>, route: &[&str], db: &str) -> SignalClient {
        SignalClient {
            sink: Arc::new(SqliteSink::open(db).expect("open sink")),
            pipeline: Arc::new(RwLock::new(Arc::new(Pipeline {
                routing: routing_to(route),
                providers: registry(providers),
            }))),
            in_flight: Arc::default(),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime"),
        }
    }

    #[test]
    fn notify_sync_times_out_and_records_the_failure() {
        let db = temp_db();
        let client = client_with(vec![Arc::new(HangingProvider)], &["hanging"], &db);

        let result = client
            .notify_sync(input(Severity::Error), Duration::from_millis(50))
            .expect("notify_sync returns a result, not an error, on timeout");

        assert_eq!(result.status, DeliveryStatus::Failed);
        assert_eq!(result.attempts, 0);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("timed out")));
        assert_eq!(stored_rows(&db), vec![("failed".to_string(), None)]);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn notify_sync_rejects_invalid_input_without_recording_it() {
        let db = temp_db();
        let client = client_with(vec![FlakyProvider::new("ok", 0)], &["ok"], &db);
        let mut bad = input(Severity::Info);
        bad.title = "  ".into();

        let err = client
            .notify_sync(bad, Duration::from_secs(1))
            .expect_err("empty title must be rejected");

        assert!(matches!(err, SignalError::Validation(_)));
        assert!(stored_rows(&db).is_empty());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn notify_async_delivers_in_the_background_and_records_it() {
        let db = temp_db();
        let ok = FlakyProvider::new("ok", 0);
        let client = client_with(vec![ok.clone()], &["ok"], &db);

        client.notify_async(input(Severity::Error));

        assert!(client.flush(Duration::from_secs(5)));
        assert_eq!(
            stored_rows(&db),
            vec![("delivered".to_string(), Some("ok".to_string()))]
        );
        assert_eq!(ok.calls(), 1);
        let _ = std::fs::remove_file(&db);
    }

    /// Sleeps for `delay` and then succeeds.
    struct SlowProvider(Duration);

    #[async_trait::async_trait]
    impl Provider for SlowProvider {
        fn name(&self) -> &'static str {
            "slow"
        }

        async fn send(&self, _signal: &Signal) -> Result<(), SignalError> {
            tokio::time::sleep(self.0).await;
            Ok(())
        }
    }

    /// Captures what the sink holds at the moment of delivery.
    struct SinkObserver {
        db: String,
        seen: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl Provider for SinkObserver {
        fn name(&self) -> &'static str {
            "observer"
        }

        async fn send(&self, _signal: &Signal) -> Result<(), SignalError> {
            *self.seen.lock().expect("seen") = stored_rows(&self.db);
            Ok(())
        }
    }

    #[test]
    fn signal_is_saved_as_pending_before_delivery() {
        let db = temp_db();
        let observer = Arc::new(SinkObserver {
            db: db.clone(),
            seen: Default::default(),
        });
        let client = client_with(vec![observer.clone()], &["observer"], &db);

        let result = client
            .notify_sync(input(Severity::Error), Duration::from_secs(5))
            .expect("notify_sync");

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(
            *observer.seen.lock().expect("seen"),
            vec![("pending".to_string(), None)]
        );
        assert_eq!(
            stored_rows(&db),
            vec![("delivered".to_string(), Some("observer".to_string()))]
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn notify_async_saves_the_signal_before_returning() {
        let db = temp_db();
        let client = client_with(
            vec![Arc::new(SlowProvider(Duration::from_millis(200)))],
            &["slow"],
            &db,
        );

        client.notify_async(input(Severity::Error));

        assert_eq!(stored_rows(&db), vec![("pending".to_string(), None)]);
        drop(client);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn flush_waits_for_background_deliveries() {
        let db = temp_db();
        let client = client_with(
            vec![Arc::new(SlowProvider(Duration::from_millis(100)))],
            &["slow"],
            &db,
        );

        client.notify_async(input(Severity::Error));
        client.notify_async(input(Severity::Error));

        assert!(client.flush(Duration::from_secs(5)));
        assert_eq!(
            stored_rows(&db),
            vec![("delivered".to_string(), Some("slow".to_string())); 2]
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn flush_gives_up_after_the_timeout_and_leaves_the_signal_pending() {
        let db = temp_db();
        let client = client_with(
            vec![Arc::new(SlowProvider(Duration::from_millis(300)))],
            &["slow"],
            &db,
        );

        client.notify_async(input(Severity::Error));

        assert!(!client.flush(Duration::from_millis(20)));
        assert_eq!(stored_rows(&db), vec![("pending".to_string(), None)]);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn dropping_the_client_finishes_background_deliveries() {
        let db = temp_db();
        let client = client_with(
            vec![Arc::new(SlowProvider(Duration::from_millis(100)))],
            &["slow"],
            &db,
        );

        client.notify_async(input(Severity::Error));
        drop(client);

        assert_eq!(
            stored_rows(&db),
            vec![("delivered".to_string(), Some("slow".to_string()))]
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn a_sink_that_cannot_record_still_lets_the_signal_through() {
        struct BrokenRecord;
        impl SignalSink for BrokenRecord {
            fn record(&self, _: &Signal) -> Result<(), SignalError> {
                Err(SignalError::Sink("disk full".into()))
            }
            fn complete(&self, _: &Signal, _: &DeliveryResult) -> Result<(), SignalError> {
                Ok(())
            }
        }
        let ok = FlakyProvider::new("ok", 0);
        let mut client = client_with(vec![ok.clone()], &["ok"], &temp_db());
        client.sink = Arc::new(BrokenRecord);

        let result = client
            .notify_sync(input(Severity::Error), Duration::from_secs(5))
            .expect("notify_sync");

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(ok.calls(), 1);
    }

    #[test]
    fn notify_async_drops_invalid_input_without_delivering() {
        let db = temp_db();
        let ok = FlakyProvider::new("ok", 0);
        let client = client_with(vec![ok.clone()], &["ok"], &db);
        let mut bad = input(Severity::Info);
        bad.source = String::new();

        client.notify_async(bad);
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(ok.calls(), 0);
        assert!(stored_rows(&db).is_empty());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn from_config_registers_only_configured_providers() {
        let db = temp_db();
        let yaml = format!(
            "providers:\n  slack:\n    webhook_url: \"https://hooks.example.test/x\"\nstorage:\n  path: \"{db}\"\n"
        );
        let client = SignalClient::from_config(&Config::from_yaml_str(&yaml).expect("config"))
            .expect("client");

        let pipeline = client.pipeline();
        let mut names: Vec<&str> = pipeline.providers.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["console", "slack"]);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn from_config_registers_pagerduty_and_teams() {
        let db = temp_db();
        let yaml = format!(
            "providers:\n  pagerduty:\n    routing_key: k\n  teams:\n    webhook_url: \"https://example.test/t\"\nstorage:\n  path: \"{db}\"\n"
        );
        let client = SignalClient::from_config(&Config::from_yaml_str(&yaml).expect("config"))
            .expect("client");

        let pipeline = client.pipeline();
        let mut names: Vec<&str> = pipeline.providers.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["console", "pagerduty", "teams"]);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn failed_pagerduty_falls_back_to_teams() {
        let server_rt = tokio::runtime::Runtime::new().expect("server runtime");
        let server = server_rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/pd"))
                .respond_with(ResponseTemplate::new(400))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/teams"))
                .respond_with(ResponseTemplate::new(202))
                .expect(1)
                .mount(&server)
                .await;
            server
        });

        let db = temp_db();
        let uri = server.uri();
        let yaml = format!(
            "providers:\n  pagerduty:\n    routing_key: k\n    url: \"{uri}/pd\"\n  teams:\n    webhook_url: \"{uri}/teams\"\nroutes:\n  - match:\n      severity: [error]\n    providers: [pagerduty, teams]\nstorage:\n  path: \"{db}\"\n"
        );
        let client = SignalClient::from_config(&Config::from_yaml_str(&yaml).expect("config"))
            .expect("client");

        let result = client
            .notify_sync(input(Severity::Error), Duration::from_secs(30))
            .expect("notify_sync");

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("teams"));
        server_rt.block_on(server.verify());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn failed_opsgenie_falls_back_to_slack() {
        let server_rt = tokio::runtime::Runtime::new().expect("server runtime");
        let server = server_rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/og"))
                .respond_with(ResponseTemplate::new(422))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/slack"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            server
        });

        let db = temp_db();
        let uri = server.uri();
        let yaml = format!(
            "providers:\n  opsgenie:\n    api_key: k\n    url: \"{uri}/og\"\n  slack:\n    webhook_url: \"{uri}/slack\"\nroutes:\n  - match:\n      severity: [error]\n    providers: [opsgenie, slack]\nstorage:\n  path: \"{db}\"\n"
        );
        let client = SignalClient::from_config(&Config::from_yaml_str(&yaml).expect("config"))
            .expect("client");

        let result = client
            .notify_sync(input(Severity::Error), Duration::from_secs(30))
            .expect("notify_sync");

        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.provider.as_deref(), Some("slack"));
        server_rt.block_on(server.verify());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn from_config_fails_when_the_sink_cannot_be_opened() {
        let yaml = "storage:\n  path: \"/nonexistent-opssignal-dir/sub/signals.db\"\n";
        let err = SignalClient::from_config(&Config::from_yaml_str(yaml).expect("config"))
            .err()
            .expect("unopenable sink must fail");
        assert!(matches!(err, SignalError::Sink(_)));
    }
}
