//! Firestore as a config source, against a mock of the emulator API.
#![cfg(feature = "gcp")]

use opssignal_core::source::ConfigSource;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const YAML: &str = "providers:\n  console: {}\n";

#[tokio::test]
async fn firestore_reads_one_string_field_of_a_document() {
    let server = MockServer::start().await;
    let host = server.uri().trim_start_matches("http://").to_string();
    std::env::set_var("FIRESTORE_EMULATOR_HOST", host);

    let doc = |database: &str, document: &str, field: &str| {
        Mock::given(method("GET"))
            .and(path(format!(
                "/v1/projects/ops/databases/{database}/documents/{document}"
            )))
            .and(query_param("mask.fieldPaths", field))
            .and(header("authorization", "Bearer owner"))
    };
    doc("(default)", "opssignal/payments", "config")
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/ops/databases/(default)/documents/opssignal/payments",
            "fields": {"config": {"stringValue": YAML}}
        })))
        .mount(&server)
        .await;
    doc("alerts", "teams/etl/configs/main", "yaml")
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "fields": {"yaml": {"stringValue": "routes: []\n"}}
        })))
        .mount(&server)
        .await;
    doc("(default)", "opssignal/missing", "config")
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let fetch = |raw: &str| {
        let source = ConfigSource::parse(raw).expect("valid source");
        async move { source.fetch().await }
    };
    assert_eq!(
        fetch("firestore://ops/opssignal/payments")
            .await
            .expect("doc"),
        YAML
    );
    assert_eq!(
        fetch("firestore://ops/teams/etl/configs/main?database=alerts&field=yaml")
            .await
            .expect("nested doc"),
        "routes: []\n"
    );
    let err = fetch("firestore://ops/opssignal/missing")
        .await
        .expect_err("missing doc");
    assert!(err.to_string().contains("not found"), "{err}");
}
