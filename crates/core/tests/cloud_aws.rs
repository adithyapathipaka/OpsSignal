//! AWS AppConfig and DynamoDB as config sources, against mock APIs.
#![cfg(feature = "aws")]

use opssignal_core::source::ConfigSource;
use wiremock::matchers::{body_partial_json, header, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const YAML: &str = "providers:\n  console: {}\n";

/// One test: the AWS environment variables are process-wide, so the
/// services run in sequence against one mock server.
#[tokio::test]
async fn aws_config_services() {
    let server = MockServer::start().await;
    std::env::set_var("AWS_ENDPOINT_URL_APPCONFIGDATA", server.uri());
    std::env::set_var("AWS_ENDPOINT_URL_DYNAMODB", server.uri());
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDTEST");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");
    std::env::set_var("AWS_REGION", "eu-west-1");

    appconfig_reuses_its_polling_session_and_renews_it(&server).await;
    dynamodb_reads_one_attribute_of_one_item(&server).await;
}

async fn appconfig_reuses_its_polling_session_and_renews_it(server: &MockServer) {
    let session = || {
        Mock::given(method("POST"))
            .and(path("/configurationsessions"))
            .and(body_partial_json(serde_json::json!({
                "ApplicationIdentifier": "opssignal",
                "EnvironmentIdentifier": "prod",
                "ConfigurationProfileIdentifier": "payments",
            })))
            // Signed with SigV4 for the appconfig service and region.
            .and(header_regex(
                "authorization",
                r"^AWS4-HMAC-SHA256 Credential=AKIDTEST/\d{8}/eu-west-1/appconfig/aws4_request",
            ))
    };
    let poll = |token: &str, next: &str, body: &str| {
        Mock::given(method("GET"))
            .and(path("/configuration"))
            .and(query_param("configuration_token", token))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Next-Poll-Configuration-Token", next)
                    .set_body_string(body),
            )
    };
    session()
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({"InitialConfigurationToken": "t1"})),
        )
        .up_to_n_times(1)
        .expect(1)
        .named("first session")
        .mount(server)
        .await;
    poll("t1", "t2", YAML).expect(1).mount(server).await;
    // Unchanged: AppConfig sends an empty body, which must not be billed
    // as a new download nor mistaken for an empty config.
    poll("t2", "t3", "").expect(1).mount(server).await;
    Mock::given(method("GET"))
        .and(path("/configuration"))
        .and(query_param("configuration_token", "t3"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"Message":"Token expired"}"#))
        .mount(server)
        .await;

    let source = ConfigSource::parse("appconfig://opssignal/prod/payments").expect("valid source");
    assert_eq!(source.fetch().await.expect("first poll"), YAML);
    assert_eq!(source.fetch().await.expect("unchanged poll"), YAML);

    // The expired token starts a fresh session.
    session()
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({"InitialConfigurationToken": "t9"})),
        )
        .expect(1)
        .named("renewed session")
        .mount(server)
        .await;
    let updated = "providers: {}\n";
    poll("t9", "t10", updated).expect(1).mount(server).await;
    assert_eq!(source.fetch().await.expect("renewed"), updated);

    // A profile with nothing deployed is an error, not an empty config.
    Mock::given(method("POST"))
        .and(path("/configurationsessions"))
        .and(body_partial_json(
            serde_json::json!({"ConfigurationProfileIdentifier": "empty"}),
        ))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({"InitialConfigurationToken": "e1"})),
        )
        .mount(server)
        .await;
    poll("e1", "e2", "").mount(server).await;
    let err = ConfigSource::parse("appconfig://opssignal/prod/empty")
        .expect("valid source")
        .fetch()
        .await
        .expect_err("nothing deployed");
    assert!(
        err.to_string().contains("no configuration deployed"),
        "{err}"
    );
}

async fn dynamodb_reads_one_attribute_of_one_item(server: &MockServer) {
    let get_item = |body: serde_json::Value| {
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-amz-target", "DynamoDB_20120810.GetItem"))
            .and(header_regex(
                "authorization",
                r"^AWS4-HMAC-SHA256 Credential=AKIDTEST/\d{8}/eu-west-1/dynamodb/aws4_request",
            ))
            .and(body_partial_json(body))
    };
    get_item(serde_json::json!({
        "TableName": "opssignal-config",
        "Key": {"id": {"S": "payments"}},
        "ExpressionAttributeNames": {"#v": "config"},
    }))
    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "Item": {"config": {"S": YAML}}
    })))
    .mount(server)
    .await;
    // Custom key and value attribute names.
    get_item(serde_json::json!({
        "Key": {"team": {"S": "etl"}},
        "ExpressionAttributeNames": {"#v": "yaml"},
    }))
    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "Item": {"yaml": {"S": "routes: []\n"}}
    })))
    .mount(server)
    .await;
    get_item(serde_json::json!({"Key": {"id": {"S": "missing"}}}))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;

    let fetch = |raw: &str| {
        let source = ConfigSource::parse(raw).expect("valid source");
        async move { source.fetch().await }
    };
    assert_eq!(
        fetch("dynamodb://opssignal-config/payments")
            .await
            .expect("item"),
        YAML
    );
    assert_eq!(
        fetch("dynamodb://opssignal-config/etl?key=team&value=yaml")
            .await
            .expect("item"),
        "routes: []\n"
    );
    let err = fetch("dynamodb://opssignal-config/missing")
        .await
        .expect_err("missing item");
    assert!(
        err.to_string().contains("no item with id = 'missing'"),
        "{err}"
    );
}
