//! Google Cloud: GCS objects, Firestore documents and Secret Manager
//! secrets.
//!
//! Credentials come from object_store's GCP chain:
//! `GOOGLE_SERVICE_ACCOUNT` / `GOOGLE_APPLICATION_CREDENTIALS`, gcloud
//! application-default credentials, then the metadata server (GKE
//! workload identity, GCE service accounts). Its tokens carry the
//! cloud-platform scope, which covers the other services too.

use super::{check_endpoint, client, config_err, fetch_object, status_err, ReqwestConnector};
use crate::signal::SignalError;
use base64::Engine;
use object_store::gcp::GoogleCloudStorageBuilder;

pub(crate) async fn fetch_gcs(bucket: &str, object: &str) -> Result<String, SignalError> {
    let store = GoogleCloudStorageBuilder::from_env()
        .with_bucket_name(bucket)
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    fetch_object(store, object).await
}

/// An OAuth bearer token from the GCP credential chain. The builder
/// needs a bucket name to build, but no request ever goes to it.
async fn token() -> Result<String, SignalError> {
    let store = GoogleCloudStorageBuilder::from_env()
        .with_bucket_name("opssignal-unused")
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    store
        .credentials()
        .get_credential()
        .await
        .map(|c| c.bearer.clone())
        .map_err(|e| config_err(format!("cannot get Google Cloud credentials: {e}")))
}

/// Reads one string field of a Firestore document. Honours
/// `FIRESTORE_EMULATOR_HOST`, which needs no credentials.
pub(crate) async fn fetch_firestore(
    project: &str,
    document: &str,
    database: Option<&str>,
    field: Option<&str>,
) -> Result<String, SignalError> {
    let (database, field) = (database.unwrap_or("(default)"), field.unwrap_or("config"));
    let (base, bearer) = match std::env::var("FIRESTORE_EMULATOR_HOST") {
        Ok(host) => (format!("http://{host}"), "owner".to_string()),
        Err(_) => (
            "https://firestore.googleapis.com".to_string(),
            token().await?,
        ),
    };
    let mut url = reqwest::Url::parse(&base).map_err(|e| config_err(e.to_string()))?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| config_err(format!("{base} cannot be a base URL")))?;
        path.extend([
            "v1",
            "projects",
            project,
            "databases",
            database,
            "documents",
        ]);
        path.extend(document.split('/'));
    }
    url.query_pairs_mut().append_pair("mask.fieldPaths", field);
    let resp = client()?
        .get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|e| config_err(e.to_string()))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(config_err(format!("document '{document}' not found")));
    }
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| config_err(format!("unexpected Firestore response: {e}")))?;
    doc.pointer(&format!("/fields/{field}/stringValue"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            config_err(format!(
                "document '{document}' has no string field '{field}' holding the config"
            ))
        })
}

/// Reads one secret version (`latest` unless pinned). Honours
/// `GOOGLE_SECRET_MANAGER_ENDPOINT` for regional endpoints and emulators.
pub(crate) async fn fetch_secret(
    project: &str,
    secret: &str,
    version: &str,
) -> Result<String, SignalError> {
    let base = match std::env::var("GOOGLE_SECRET_MANAGER_ENDPOINT") {
        Ok(url) => {
            check_endpoint(&url)?;
            url.trim_end_matches('/').to_string()
        }
        Err(_) => "https://secretmanager.googleapis.com".to_string(),
    };
    let mut url = reqwest::Url::parse(&base).map_err(|e| config_err(e.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| config_err(format!("{base} cannot be a base URL")))?
        .extend(["v1", "projects", project, "secrets", secret, "versions"])
        .push(&format!("{version}:access"));
    let resp = client()?
        .get(url)
        .bearer_auth(token().await?)
        .send()
        .await
        .map_err(|e| config_err(e.to_string()))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| config_err(format!("unexpected Secret Manager response: {e}")))?;
    let data = body
        .pointer("/payload/data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| config_err("Secret Manager sent no payload"))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| config_err(format!("Secret Manager payload is not base64: {e}")))?;
    String::from_utf8(raw).map_err(|_| config_err("the secret is not valid UTF-8"))
}
