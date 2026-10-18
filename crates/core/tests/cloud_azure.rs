//! Azure App Configuration and Cosmos DB as config sources, against
//! mock Entra ID and service endpoints.
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
    // A different audience from App Configuration's, and `{account}`
    // filled in.
    std::env::set_var(
        "AZURE_COSMOS_ENDPOINT",
        format!("{}/cosmos/{{account}}", server.uri()),
    );
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
        // One per service audience, then cached.
        .expect(2)
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

    cosmos_reads_an_item_with_an_entra_token(&server).await;
}

async fn cosmos_reads_an_item_with_an_entra_token(server: &MockServer) {
    let item = |pk: &str| {
        Mock::given(method("GET"))
            .and(path("/cosmos/acct/dbs/ops/colls/routing/docs/payments"))
            .and(header(
                "authorization",
                "type%3Daad%26ver%3D1.0%26sig%3Dtok",
            ))
            .and(header("x-ms-documentdb-partitionkey", pk))
            .and(header("x-ms-version", "2018-12-31"))
    };
    item("[\"payments\"]")
        .and(header("if-none-match", "\"0b00\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(server)
        .await;
    item("[\"payments\"]")
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "payments", "config": YAML, "_etag": "\"0b00\""
        })))
        .expect(1)
        .mount(server)
        .await;
    // An explicit partition key value, and a custom property.
    item("[\"team-a\"]")
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "payments", "yaml": "routes: []\n"
        })))
        .mount(server)
        .await;

    let source = ConfigSource::parse("cosmos://acct/ops/routing/payments").expect("valid source");
    assert_eq!(source.fetch().await.expect("item"), YAML);
    assert_eq!(source.fetch().await.expect("unchanged"), YAML);
    let custom = ConfigSource::parse("cosmos://acct/ops/routing/payments?pk=team-a&field=yaml")
        .expect("valid source");
    assert_eq!(custom.fetch().await.expect("item"), "routes: []\n");
}
