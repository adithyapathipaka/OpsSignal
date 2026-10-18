//! AWS: S3 objects and AppConfig configurations.
//!
//! Credentials come from object_store's AWS chain: static keys, web
//! identity (EKS IRSA), ECS task roles and EC2 instance profiles. The
//! same credentials sign the non-S3 calls with SigV4, so there is no
//! AWS SDK in the tree.

use super::{config_err, fetch_object, send, status_err, utf8, ReqwestConnector};
use crate::signal::SignalError;
use object_store::aws::{AmazonS3Builder, AwsAuthorizer, AwsCredential};
use object_store::client::{HttpRequest, HttpRequestBody};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) async fn fetch_s3(bucket: &str, key: &str) -> Result<String, SignalError> {
    // from_env picks up AWS_REGION, AWS_ENDPOINT_URL, static keys,
    // web identity (EKS IRSA), ECS task roles and EC2 instance profiles.
    let store = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    fetch_object(store, key).await
}

/// Resolves credentials through object_store's chain. The builder needs
/// a bucket name to build, but no request ever goes to it.
async fn credential() -> Result<Arc<AwsCredential>, SignalError> {
    let store = AmazonS3Builder::from_env()
        .with_bucket_name("opssignal-unused")
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    store
        .credentials()
        .get_credential()
        .await
        .map_err(|e| config_err(format!("cannot get AWS credentials: {e}")))
}

fn region() -> String {
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string())
}

/// The service endpoint, honouring the SDKs' `AWS_ENDPOINT_URL_<SERVICE>`
/// and `AWS_ENDPOINT_URL` overrides (LocalStack, VPC endpoints).
fn endpoint(service_env: &str, host: &str) -> Result<String, SignalError> {
    let custom = std::env::var(format!("AWS_ENDPOINT_URL_{service_env}"))
        .or_else(|_| std::env::var("AWS_ENDPOINT_URL"));
    match custom {
        Ok(url) => {
            super::check_endpoint(&url)?;
            Ok(url.trim_end_matches('/').to_string())
        }
        Err(_) => Ok(format!("https://{host}.{}.amazonaws.com", region())),
    }
}

type Response = (
    reqwest::StatusCode,
    reqwest::header::HeaderMap,
    bytes::Bytes,
);

/// A SigV4-signed call to `service`.
async fn call(
    service: &str,
    method: http::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Result<Response, SignalError> {
    let credential = credential().await?;
    let region = region();
    let mut builder = http::Request::builder().method(method).uri(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request: HttpRequest = builder
        .body(HttpRequestBody::from(body))
        .map_err(|e| config_err(e.to_string()))?;
    AwsAuthorizer::new(&credential, service, &region)
        .try_authorize(&mut request, None)
        .map_err(|e| config_err(format!("cannot sign the request: {e}")))?;
    send(request).await
}

/// Where each AppConfig profile's polling session stands: the token for
/// the next poll and the configuration last received.
type Sessions = Mutex<HashMap<String, (String, String)>>;

fn sessions() -> &'static Sessions {
    static SESSIONS: OnceLock<Sessions> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

/// Reads a configuration profile through the AppConfig Data API.
///
/// AppConfig bills for every configuration it sends, so the polling
/// session is kept between calls: a poll on an unchanged profile gets an
/// empty, unbilled answer and the last copy is returned. An expired or
/// rejected session starts a new one.
pub(crate) async fn fetch_appconfig(
    application: &str,
    environment: &str,
    profile: &str,
) -> Result<String, SignalError> {
    let id = format!("{application}/{environment}/{profile}");
    let base = endpoint("APPCONFIGDATA", "appconfigdata")?;
    let previous = sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    if let Some((token, text)) = previous {
        match latest(&base, &token).await {
            Ok((next, body)) => {
                let text = if body.is_empty() { text } else { utf8(&body)? };
                remember(&id, next, &text);
                return Ok(text);
            }
            Err(e) => tracing::debug!("AppConfig session for {id} ended ({e}); starting a new one"),
        }
    }
    let token = start_session(&base, application, environment, profile).await?;
    let (next, body) = latest(&base, &token).await?;
    if body.is_empty() {
        return Err(config_err(
            "AppConfig has no configuration deployed to this environment",
        ));
    }
    let text = utf8(&body)?;
    remember(&id, next, &text);
    Ok(text)
}

fn remember(id: &str, token: String, text: &str) {
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string(), (token, text.to_string()));
}

async fn start_session(
    base: &str,
    application: &str,
    environment: &str,
    profile: &str,
) -> Result<String, SignalError> {
    let body = serde_json::json!({
        "ApplicationIdentifier": application,
        "EnvironmentIdentifier": environment,
        "ConfigurationProfileIdentifier": profile,
    });
    let (status, _, bytes) = call(
        "appconfig",
        http::Method::POST,
        &format!("{base}/configurationsessions"),
        &[("content-type", "application/json")],
        body.to_string().into_bytes(),
    )
    .await?;
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Session {
        initial_configuration_token: String,
    }
    serde_json::from_slice::<Session>(&bytes)
        .map(|s| s.initial_configuration_token)
        .map_err(|e| config_err(format!("unexpected AppConfig response: {e}")))
}

/// One `GetLatestConfiguration` call: the next token, and the body,
/// which is empty when nothing changed since `token` was issued.
async fn latest(base: &str, token: &str) -> Result<(String, bytes::Bytes), SignalError> {
    let mut url = reqwest::Url::parse(&format!("{base}/configuration"))
        .map_err(|e| config_err(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("configuration_token", token);
    let (status, headers, bytes) = call(
        "appconfig",
        http::Method::GET,
        url.as_str(),
        &[],
        Vec::new(),
    )
    .await?;
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    let next = headers
        .get("next-poll-configuration-token")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| config_err("AppConfig sent no Next-Poll-Configuration-Token"))?;
    Ok((next.to_string(), bytes))
}
