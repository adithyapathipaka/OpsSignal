//! `${store:...}` secret placeholders, resolved through each cloud's
//! secret store (mocked), picked up again when rotated, and cached.

use opssignal_core::signal::{Severity, SignalInput};
use opssignal_core::source::{ConfigSource, LoadOptions};
use opssignal_core::SignalClient;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The mock server needs its own runtime: `from_source` blocks on the
/// client's runtime, and the two must not be nested.
#[allow(dead_code)]
struct Server {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

#[allow(dead_code)]
impl Server {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("server runtime");
        let server = rt.block_on(MockServer::start());
        let s = Server { rt, server };
        // Every webhook delivery succeeds; tests check where it went.
        s.mount(
            Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path_regex("^/hook-"))
                .respond_with(ResponseTemplate::new(200)),
        );
        s
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(mock.mount(&self.server));
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    /// Requests received so far whose path is `path`.
    fn hits(&self, path: &str) -> usize {
        self.rt
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == path)
            .count()
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "opssignal-secrets-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A client on a local file that sends every signal to a webhook whose
/// URL is `url` (usually a secret placeholder).
fn client(dir: &Path, url: &str, extra: &str) -> Result<SignalClient, String> {
    let file = dir.join("signal.yaml");
    std::fs::write(
        &file,
        format!(
            "providers:\n  webhook:\n    url: \"{url}\"\n{extra}routes:\n  - match: {{}}\n    providers: [webhook]\nstorage:\n  path: \"{}\"\n",
            dir.join("signals.db").display()
        ),
    )
    .expect("test fixture");
    let opts = LoadOptions {
        refresh: Some(Duration::from_millis(30)),
        cache_dir: Some(dir.join("cache")),
    };
    SignalClient::from_source(&ConfigSource::File(file), &opts).map_err(|e| e.to_string())
}

#[allow(dead_code)]
fn notify(client: &SignalClient) {
    let input = SignalInput {
        source: "ci".into(),
        event_type: "build_failed".into(),
        severity: Severity::Error,
        title: "main is red".into(),
        ..Default::default()
    };
    let _ = client.notify_sync(input, Duration::from_secs(5));
}

/// Notifies until a delivery reaches `path`, as a reload lands.
#[allow(dead_code)]
fn eventually_delivers_to(client: &SignalClient, server: &Server, path: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        notify(client);
        if server.hits(path) > 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

#[allow(dead_code)]
fn provider_names(client: &SignalClient) -> Vec<String> {
    let mut names: Vec<String> = client.pipeline().providers.keys().cloned().collect();
    names.sort_unstable();
    names
}

#[test]
fn unknown_secret_store_stops_startup() {
    let dir = scratch("unknown");
    let err = client(&dir, "${vault:prod/slack}", "")
        .err()
        .expect("fatal");
    assert!(err.contains("unknown secret store 'vault'"), "{err}");
}

#[cfg(not(feature = "aws"))]
#[test]
fn missing_add_on_stops_startup() {
    let dir = scratch("no-addon");
    let err = client(&dir, "${aws-sm:prod/slack}", "")
        .err()
        .expect("fatal");
    assert!(err.contains("pip install 'opssignal[aws]'"), "{err}");
    assert!(err.contains("AWS Secrets Manager"), "{err}");
}

/// One test: the AWS variables and the TTL are process-wide.
#[cfg(feature = "aws")]
#[test]
fn aws_secrets_manager_secrets_resolve_rotate_and_are_cached() {
    use wiremock::matchers::{body_partial_json, header, header_regex, method, path};

    let server = Server::start();
    std::env::set_var("AWS_ENDPOINT_URL_SECRETSMANAGER", server.uri());
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDTEST");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");
    std::env::set_var("AWS_REGION", "eu-west-1");
    std::env::set_var("SIGNAL_SECRETS_TTL_SECS", "0");

    let get_secret = |id: &str, region: &str| {
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-amz-target", "secretsmanager.GetSecretValue"))
            .and(header_regex(
                "authorization",
                &format!(
                    r"^AWS4-HMAC-SHA256 Credential=AKIDTEST/\d{{8}}/{region}/secretsmanager/aws4_request"
                ),
            ))
            .and(body_partial_json(serde_json::json!({ "SecretId": id })))
    };
    let secret = |value: String| {
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "Name": "x", "SecretString": value }))
    };
    let hook = |n: u8| {
        serde_json::json!({ "url": format!("{}/hook-v{n}", server.uri()), "user": "ops" })
            .to_string()
    };
    server.mount(
        get_secret("prod/webhook", "eu-west-1")
            .respond_with(secret(hook(1)))
            .up_to_n_times(1),
    );
    // A full ARN is signed for, and sent to, its own region.
    server.mount(
        get_secret(
            "arn:aws:secretsmanager:eu-central-1:123456789012:secret:slack-AbC",
            "eu-central-1",
        )
        .respond_with(secret("https://hooks.example.test/s".into())),
    );

    let dir = scratch("aws");
    let slack = "  slack:\n    webhook_url: \"${aws-sm:arn:aws:secretsmanager:eu-central-1:123456789012:secret:slack-AbC}\"\n";
    let first = client(&dir, "${aws-sm:prod/webhook#url}", slack).expect("client");
    assert_eq!(provider_names(&first), ["console", "slack", "webhook"]);
    notify(&first);
    assert_eq!(server.hits("/hook-v1"), 1, "delivered to the secret URL");

    // Rotated: the next reload rebuilds the pipeline with the new value,
    // though the file itself never changed.
    server.mount(get_secret("prod/webhook", "eu-west-1").respond_with(secret(hook(2))));
    assert!(
        eventually_delivers_to(&first, &server, "/hook-v2"),
        "rotation applied"
    );
    // The cached file still has the placeholder, never the value.
    for entry in std::fs::read_dir(dir.join("cache")).into_iter().flatten() {
        let text = std::fs::read_to_string(entry.expect("entry").path()).expect("cache");
        assert!(!text.contains("hook-v"), "secret written to the cache");
    }
    drop(first);

    // With a TTL, reloads reuse the value instead of reading it again.
    std::env::set_var("SIGNAL_SECRETS_TTL_SECS", "3600");
    server.mount(get_secret("prod/cached", "eu-west-1").respond_with(secret(hook(3))));
    let dir = scratch("aws-ttl");
    let cached = client(&dir, "${aws-sm:prod/cached#url}", "").expect("client");
    std::thread::sleep(Duration::from_millis(300));
    let reads = server
        .rt
        .block_on(server.server.received_requests())
        .unwrap_or_default()
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("prod/cached"))
        .count();
    assert_eq!(reads, 1, "one read across several reloads");
    drop(cached);

    // A store outage is not fatal: the client starts on the default.
    server.mount(
        get_secret("prod/down", "eu-west-1")
            .respond_with(ResponseTemplate::new(500).set_body_string("{}")),
    );
    let dir = scratch("aws-down");
    let down = client(&dir, "${aws-sm:prod/down}", "").expect("starts anyway");
    assert_eq!(provider_names(&down), ["console"]);
}

#[cfg(feature = "gcp")]
#[test]
fn gcp_secret_manager_secret_is_resolved() {
    use base64::Engine;
    use wiremock::matchers::{header, method, path};

    let server = Server::start();
    std::env::set_var("GOOGLE_SECRET_MANAGER_ENDPOINT", server.uri());
    std::env::set_var("GOOGLE_BEARER_TOKEN", "gtok");
    let url = format!("{}/hook-gcp", server.uri());
    server.mount(
        Mock::given(method("GET"))
            .and(path(
                "/v1/projects/ops/secrets/webhook/versions/latest:access",
            ))
            .and(header("authorization", "Bearer gtok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "projects/1/secrets/webhook/versions/4",
                "payload": {
                    "data": base64::engine::general_purpose::STANDARD.encode(&url)
                }
            }))),
    );

    let dir = scratch("gcp");
    let client = client(&dir, "${gcp-sm:ops/webhook}", "").expect("client");
    notify(&client);
    assert_eq!(server.hits("/hook-gcp"), 1);
}

#[cfg(feature = "azure")]
#[test]
fn azure_key_vault_secret_is_resolved() {
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};

    let server = Server::start();
    std::env::set_var("AZURE_AUTHORITY_HOST", server.uri());
    std::env::set_var(
        "AZURE_KEYVAULT_ENDPOINT",
        format!("{}/kv/{{vault}}", server.uri()),
    );
    std::env::set_var("AZURE_TENANT_ID", "tenant");
    std::env::set_var("AZURE_CLIENT_ID", "client");
    std::env::set_var("AZURE_CLIENT_SECRET", "secret");
    std::env::remove_var("AZURE_FEDERATED_TOKEN_FILE");
    server.mount(
        Mock::given(method("POST"))
            .and(path("/tenant/oauth2/v2.0/token"))
            .and(body_string_contains(format!(
                "scope={}",
                url_encode(&format!("{}/kv/ops-vault/.default", server.uri()))
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ktok", "expires_in": 3600, "token_type": "Bearer"
            }))),
    );
    server.mount(
        Mock::given(method("GET"))
            .and(path("/kv/ops-vault/secrets/webhook-url/7f3a"))
            .and(query_param("api-version", "7.4"))
            .and(header("authorization", "Bearer ktok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "https://ops-vault.vault.azure.net/secrets/webhook-url/7f3a",
                "value": format!("{}/hook-azure", server.uri()),
            }))),
    );

    let dir = scratch("azure");
    let client = client(&dir, "${azure-kv:ops-vault/webhook-url/7f3a}", "").expect("client");
    notify(&client);
    assert_eq!(server.hits("/hook-azure"), 1);
}

#[cfg(feature = "azure")]
fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
