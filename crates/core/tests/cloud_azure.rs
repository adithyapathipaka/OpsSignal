//! Azure App Configuration as a config source, against mock Entra ID
//! and App Configuration endpoints.
#![cfg(feature = "azure")]

use opssignal_core::source::ConfigSource;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const YAML: &str = "providers:\n  console: {}\n";

/// One test: the Azure environment variables are process-wide.
#[tokio::test]
async fn app_configuration_uses_a_service_principal_token_and_etags() {
    let server = MockServer::start().await;
    std::env::set_var("AZURE_AUTHORITY_HOST", server.uri());
    std::env::set_var("AZURE_APPCONFIG_ENDPOINT", server.uri());
    std::env::set_var("AZURE_TENANT_ID", "tenant");
    std::env::set_var("AZURE_CLIENT_ID", "client");
    std::env::set_var("AZURE_CLIENT_SECRET", "secret");
    std::env::remove_var("AZURE_FEDERATED_TOKEN_FILE");

    Mock::given(method("POST"))
        .and(path("/tenant/oauth2/v2.0/token"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains("client_secret=secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tok", "expires_in": 3600, "token_type": "Bearer"
        })))
        // Cached for the second fetch.
        .expect(1)
        .mount(&server)
        .await;
    let kv = || {
        Mock::given(method("GET"))
            // The key's slashes are one path segment.
            .and(path("/kv/opssignal%2Fteams%2Fpayments"))
            .and(query_param("label", "prod"))
            .and(query_param("api-version", "1.0"))
            .and(header("authorization", "Bearer tok"))
    };
    kv().and(header("if-none-match", "\"e1\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .named("conditional")
        .mount(&server)
        .await;
    kv().respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "key": "opssignal/teams/payments", "label": "prod", "value": YAML, "etag": "e1"
    })))
    .expect(1)
    .named("first")
    .mount(&server)
    .await;

    let source = ConfigSource::parse("azappconfig://ops/opssignal/teams/payments?label=prod")
        .expect("valid source");
    assert_eq!(source.fetch().await.expect("first fetch"), YAML);
    assert_eq!(source.fetch().await.expect("unchanged"), YAML);

    let err = ConfigSource::parse("azappconfig://ops/missing")
        .expect("valid source")
        .fetch()
        .await
        .expect_err("missing key");
    assert!(
        err.to_string()
            .contains("key 'missing' (no label) not found"),
        "{err}"
    );
}
