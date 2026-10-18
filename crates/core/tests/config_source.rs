//! Remote config sources, the local cache fallback, and live reload,
//! driven through `SignalClient::from_source` the way bindings use it.

use opssignal_core::source::{ConfigSource, LoadOptions};
use opssignal_core::SignalClient;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The mock server needs its own runtime: `from_source` blocks on the
/// client's runtime, and the two must not be nested.
struct Server {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

impl Server {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("server runtime");
        let server = rt.block_on(MockServer::start());
        Server { rt, server }
    }

    fn serve(&self, route: &str, status: u16, body: &str) {
        self.serve_regex(&format!("^{}$", regex_escape(route)), status, body);
    }

    fn serve_regex(&self, route: &str, status: u16, body: &str) {
        self.rt.block_on(async {
            self.server.reset().await;
            Mock::given(method("GET"))
                .and(path_regex(route))
                .respond_with(
                    ResponseTemplate::new(status)
                        // Object stores send these; GCS requires them.
                        .insert_header("Last-Modified", "Sun, 18 Oct 2026 12:00:00 GMT")
                        .insert_header("ETag", "\"1\"")
                        .set_body_string(body),
                )
                .mount(&self.server)
                .await;
        });
    }

    fn url(&self, route: &str) -> String {
        format!("{}{route}", self.server.uri())
    }
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let escape = !c.is_ascii_alphanumeric() && c != '/';
            escape.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "opssignal-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn opts(dir: &Path, refresh: Option<Duration>) -> LoadOptions {
    LoadOptions {
        refresh,
        cache_dir: Some(dir.join("cache")),
    }
}

fn webhook_yaml(dir: &Path, url: &str) -> String {
    format!(
        "providers:\n  webhook:\n    url: \"{url}\"\nstorage:\n  path: \"{}\"\n",
        dir.join("signals.db").display()
    )
}

fn console_yaml(dir: &Path) -> String {
    format!(
        "storage:\n  path: \"{}\"\n",
        dir.join("signals.db").display()
    )
}

fn provider_names(client: &SignalClient) -> Vec<String> {
    let mut names: Vec<String> = client.pipeline().providers.keys().cloned().collect();
    names.sort_unstable();
    names
}

fn wait_for(client: &SignalClient, want: &[&str]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if provider_names(client) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn https_source_is_loaded_and_cached_before_interpolation() {
    std::env::set_var("OPSSIGNAL_IT_HOOK", "https://hooks.example.test/secret");
    let dir = scratch("https");
    let server = Server::start();
    let yaml = webhook_yaml(&dir, "${OPSSIGNAL_IT_HOOK}");
    server.serve("/signal.yaml", 200, &yaml);
    let source = ConfigSource::parse(&server.url("/signal.yaml")).expect("source");
    let opts = opts(&dir, None);

    let client = SignalClient::from_source(&source, &opts).expect("client");

    assert_eq!(provider_names(&client), ["console", "webhook"]);
    let cached = std::fs::read_to_string(opts.cache_path(&source).expect("cache path"))
        .expect("cache written");
    assert_eq!(cached, yaml, "cache holds the raw text");
    assert!(!cached.contains("secret"), "no interpolated secret on disk");
}

#[test]
fn unreachable_source_falls_back_to_the_cached_copy() {
    let dir = scratch("fallback");
    let server = Server::start();
    server.serve("/signal.yaml", 503, "");
    let source = ConfigSource::parse(&server.url("/signal.yaml")).expect("source");
    let opts = opts(&dir, None);
    let cache = opts.cache_path(&source).expect("cache path");
    std::fs::create_dir_all(cache.parent().expect("test fixture")).expect("test fixture");
    std::fs::write(&cache, webhook_yaml(&dir, "https://hooks.example.test/x"))
        .expect("test fixture");

    let client = SignalClient::from_source(&source, &opts).expect("client");

    assert_eq!(provider_names(&client), ["console", "webhook"]);
}

#[test]
fn broken_remote_file_falls_back_to_the_cached_copy() {
    let dir = scratch("broken");
    let server = Server::start();
    server.serve("/signal.yaml", 200, "providers:\n  slak: {}\n");
    let source = ConfigSource::parse(&server.url("/signal.yaml")).expect("source");
    let opts = opts(&dir, None);
    let cache = opts.cache_path(&source).expect("cache path");
    std::fs::create_dir_all(cache.parent().expect("test fixture")).expect("test fixture");
    std::fs::write(&cache, webhook_yaml(&dir, "https://hooks.example.test/x"))
        .expect("test fixture");

    let client = SignalClient::from_source(&source, &opts).expect("client");

    assert_eq!(provider_names(&client), ["console", "webhook"]);
    let still = std::fs::read_to_string(&cache).expect("test fixture");
    assert!(
        still.contains("webhook"),
        "a broken fetch never overwrites the cache"
    );
}

#[test]
fn no_source_and_no_cache_starts_on_console_only() {
    let dir = scratch("nocache");
    let server = Server::start();
    server.serve("/signal.yaml", 500, "");
    let source = ConfigSource::parse(&server.url("/signal.yaml")).expect("source");

    let client = SignalClient::from_source(&source, &opts(&dir, None)).expect("client");

    assert_eq!(provider_names(&client), ["console"]);
}

#[test]
fn missing_local_file_is_still_an_error() {
    let dir = scratch("missing");
    let source = ConfigSource::File(dir.join("nope.yaml"));
    let err = SignalClient::from_source(&source, &opts(&dir, None))
        .err()
        .expect("missing local file must fail");
    assert!(err.to_string().contains("nope.yaml"));
}

#[test]
fn local_file_changes_are_applied_without_a_restart() {
    let dir = scratch("reload-file");
    let file = dir.join("signal.yaml");
    std::fs::write(&file, console_yaml(&dir)).expect("test fixture");
    let source = ConfigSource::File(file.clone());

    let client = SignalClient::from_source(&source, &opts(&dir, Some(Duration::from_millis(30))))
        .expect("client");
    assert_eq!(provider_names(&client), ["console"]);

    std::fs::write(&file, webhook_yaml(&dir, "https://hooks.example.test/x"))
        .expect("test fixture");
    assert!(wait_for(&client, &["console", "webhook"]), "edit applied");

    // A broken edit is rejected and the last good pipeline stays.
    std::fs::write(&file, "routes: [unclosed\n").expect("test fixture");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(provider_names(&client), ["console", "webhook"]);

    // So is the file disappearing for a moment, as during a ConfigMap swap.
    std::fs::remove_file(&file).expect("test fixture");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(provider_names(&client), ["console", "webhook"]);

    std::fs::write(&file, console_yaml(&dir)).expect("test fixture");
    assert!(
        wait_for(&client, &["console"]),
        "recovers on the next good edit"
    );
}

#[test]
fn remote_changes_are_applied_and_cached() {
    let dir = scratch("reload-remote");
    let server = Server::start();
    server.serve("/signal.yaml", 200, &console_yaml(&dir));
    let source = ConfigSource::parse(&server.url("/signal.yaml")).expect("source");
    let opts = opts(&dir, Some(Duration::from_millis(30)));

    let client = SignalClient::from_source(&source, &opts).expect("client");
    assert_eq!(provider_names(&client), ["console"]);

    let updated = webhook_yaml(&dir, "https://hooks.example.test/x");
    server.serve("/signal.yaml", 200, &updated);
    assert!(
        wait_for(&client, &["console", "webhook"]),
        "remote edit applied"
    );
    let cache = opts.cache_path(&source).expect("test fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(&cache).ok().as_deref() != Some(updated.as_str()) {
        assert!(Instant::now() < deadline, "cache refreshed after reload");
        std::thread::sleep(Duration::from_millis(20));
    }

    // An outage after that keeps the current config.
    server.serve("/signal.yaml", 503, "");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(provider_names(&client), ["console", "webhook"]);
}

#[test]
fn starting_on_defaults_picks_up_the_source_once_it_is_back() {
    let dir = scratch("recover");
    let server = Server::start();
    server.serve("/signal.yaml", 503, "");
    let source = ConfigSource::parse(&server.url("/signal.yaml")).expect("source");

    let client = SignalClient::from_source(&source, &opts(&dir, Some(Duration::from_millis(30))))
        .expect("client");
    assert_eq!(provider_names(&client), ["console"]);

    server.serve(
        "/signal.yaml",
        200,
        &webhook_yaml(&dir, "https://hooks.example.test/x"),
    );
    assert!(wait_for(&client, &["console", "webhook"]));
}

/// Talks to a mock S3 endpoint with static keys, which is how the
/// standard AWS credential chain resolves when they are in the env.
#[cfg(feature = "s3")]
#[test]
fn s3_source_reads_the_object_with_the_standard_aws_env() {
    let dir = scratch("s3");
    let server = Server::start();
    server.serve(
        "/ops-config/prod/signal.yaml",
        200,
        &webhook_yaml(&dir, "https://hooks.example.test/x"),
    );
    // This test binary is the only one that sets AWS_* variables.
    std::env::set_var("AWS_ENDPOINT_URL", server.server.uri());
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    std::env::set_var("AWS_ACCESS_KEY_ID", "test");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
    std::env::set_var("AWS_REGION", "us-east-1");
    let source = ConfigSource::parse("s3://ops-config/prod/signal.yaml").expect("source");

    let client = SignalClient::from_source(&source, &opts(&dir, None)).expect("client");

    assert_eq!(provider_names(&client), ["console", "webhook"]);
}

/// Same for GCS, with a static bearer token standing in for workload
/// identity.
#[cfg(feature = "gcs")]
#[test]
fn gcs_source_reads_the_object_with_the_standard_google_env() {
    let dir = scratch("gcs");
    let server = Server::start();
    // GCS percent-encodes every non-alphanumeric byte of the bucket and
    // object names.
    server.serve_regex(
        "^/ops%2Dconfig/prod%2Fsignal%2Eyaml$",
        200,
        &webhook_yaml(&dir, "https://hooks.example.test/x"),
    );
    std::env::set_var("GOOGLE_BASE_URL", server.server.uri());
    std::env::set_var("GOOGLE_BEARER_TOKEN", "test");
    let source = ConfigSource::parse("gs://ops-config/prod/signal.yaml").expect("source");

    let client = SignalClient::from_source(&source, &opts(&dir, None)).expect("client");

    assert_eq!(provider_names(&client), ["console", "webhook"]);
}
