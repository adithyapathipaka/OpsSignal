//! Azure: Blob Storage objects and App Configuration key-values.
//!
//! Blob Storage uses object_store's own credential chain. The other
//! services need Microsoft Entra tokens for their own audiences, which
//! object_store only issues for storage, so [`token`] gets them the same
//! way, in the same order: AKS workload identity, a service principal
//! secret, then the managed identity endpoint.

use super::{check_endpoint, client, config_err, fetch_object, status_err, ReqwestConnector};
use crate::signal::SignalError;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) async fn fetch_blob(container: &str, blob: &str) -> Result<String, SignalError> {
    // from_env picks up AZURE_STORAGE_ACCOUNT_NAME plus an account key,
    // SAS token, service principal, AKS workload identity
    // (AZURE_FEDERATED_TOKEN_FILE), then the managed identity endpoint.
    let store = object_store::azure::MicrosoftAzureBuilder::from_env()
        .with_container_name(container)
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    fetch_object(store, blob).await
}

/// Cached tokens by scope, with when they stop being usable.
type Tokens = Mutex<HashMap<String, (String, Instant)>>;

fn tokens() -> &'static Tokens {
    static TOKENS: OnceLock<Tokens> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

/// A bearer token for `scope` (such as `https://azconfig.io/.default`).
pub(crate) async fn token(scope: &str) -> Result<String, SignalError> {
    if let Some((token, until)) = tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(scope)
    {
        if Instant::now() < *until {
            return Ok(token.clone());
        }
    }
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (client_id, tenant) = (env("AZURE_CLIENT_ID"), env("AZURE_TENANT_ID"));
    let (token, expires_in) = match (&client_id, &tenant) {
        (Some(client_id), Some(tenant)) if env("AZURE_FEDERATED_TOKEN_FILE").is_some() => {
            let path = env("AZURE_FEDERATED_TOKEN_FILE").unwrap_or_default();
            let assertion = std::fs::read_to_string(&path)
                .map_err(|e| config_err(format!("cannot read {path}: {e}")))?;
            entra(
                tenant,
                &[
                    ("client_id", client_id),
                    ("scope", scope),
                    ("grant_type", "client_credentials"),
                    (
                        "client_assertion_type",
                        "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                    ),
                    ("client_assertion", assertion.trim()),
                ],
            )
            .await?
        }
        (Some(client_id), Some(tenant)) if env("AZURE_CLIENT_SECRET").is_some() => {
            let secret = env("AZURE_CLIENT_SECRET").unwrap_or_default();
            entra(
                tenant,
                &[
                    ("client_id", client_id),
                    ("client_secret", &secret),
                    ("scope", scope),
                    ("grant_type", "client_credentials"),
                ],
            )
            .await?
        }
        _ => managed_identity(scope, client_id.as_deref()).await?,
    };
    // Refresh a minute early so a token never expires mid-request.
    let until = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
    tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(scope.to_string(), (token.clone(), until));
    Ok(token)
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    /// A number from Entra ID, a string from the managed identity
    /// endpoint.
    #[serde(deserialize_with = "number_or_string")]
    expires_in: u64,
}

fn number_or_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Value {
        Number(u64),
        Text(String),
    }
    match <Value as serde::Deserialize>::deserialize(d)? {
        Value::Number(n) => Ok(n),
        Value::Text(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

async fn entra(tenant: &str, form: &[(&str, &str)]) -> Result<(String, u64), SignalError> {
    let authority = std::env::var("AZURE_AUTHORITY_HOST")
        .unwrap_or_else(|_| "https://login.microsoftonline.com".to_string());
    check_endpoint(&authority)?;
    let url = format!(
        "{}/{tenant}/oauth2/v2.0/token",
        authority.trim_end_matches('/')
    );
    let resp = client()?
        .post(&url)
        .form(form)
        .send()
        .await
        .map_err(|e| config_err(format!("cannot get an Azure token: {e}")))?;
    read_token(resp).await
}

async fn managed_identity(
    scope: &str,
    client_id: Option<&str>,
) -> Result<(String, u64), SignalError> {
    let resource = scope.trim_end_matches("/.default");
    let mut query = vec![("api-version", "2018-02-01"), ("resource", resource)];
    if let Some(id) = client_id {
        query.push(("client_id", id));
    }
    let resp = client()?
        .get("http://169.254.169.254/metadata/identity/oauth2/token")
        .header("Metadata", "true")
        .query(&query)
        .send()
        .await
        .map_err(|e| {
            config_err(format!(
                "no Azure credentials: set AZURE_CLIENT_ID and AZURE_TENANT_ID with \
                 workload identity or AZURE_CLIENT_SECRET, or run with a managed identity ({e})"
            ))
        })?;
    read_token(resp).await
}

async fn read_token(resp: reqwest::Response) -> Result<(String, u64), SignalError> {
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    serde_json::from_slice::<TokenResponse>(&bytes)
        .map(|t| (t.access_token, t.expires_in))
        .map_err(|e| config_err(format!("unexpected Azure token response: {e}")))
}

/// Last value and ETag of each App Configuration key, so an unchanged
/// key costs a 304 instead of a full download.
type Etags = Mutex<HashMap<String, (String, String)>>;

fn etags() -> &'static Etags {
    static ETAGS: OnceLock<Etags> = OnceLock::new();
    ETAGS.get_or_init(Default::default)
}

/// Reads one key-value from an App Configuration store. A key with no
/// `label` reads the unlabelled value.
pub(crate) async fn fetch_app_config(
    store: &str,
    key: &str,
    label: Option<&str>,
) -> Result<String, SignalError> {
    // AZURE_APPCONFIG_ENDPOINT, with `{store}` in it, covers sovereign
    // clouds and local emulators.
    let (endpoint, scope) = match std::env::var("AZURE_APPCONFIG_ENDPOINT") {
        Ok(template) => {
            let endpoint = template.replace("{store}", store);
            check_endpoint(&endpoint)?;
            let endpoint = endpoint.trim_end_matches('/').to_string();
            let scope = format!("{endpoint}/.default");
            (endpoint, scope)
        }
        Err(_) => (
            format!("https://{store}.azconfig.io"),
            "https://azconfig.io/.default".to_string(),
        ),
    };
    let mut url = reqwest::Url::parse(&endpoint).map_err(|e| config_err(e.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| config_err(format!("{endpoint} cannot be a base URL")))?
        .push("kv")
        .push(key);
    url.query_pairs_mut()
        // "\0" is how the API spells "no label".
        .append_pair("label", label.unwrap_or("\0"))
        .append_pair("api-version", "1.0");

    let id = url.to_string();
    let cached = etags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    let mut request = client()?
        .get(url)
        .bearer_auth(token(&scope).await?)
        .header("Accept", "application/vnd.microsoft.appconfig.kv+json");
    if let Some((etag, _)) = &cached {
        request = request.header("If-None-Match", format!("\"{etag}\""));
    }
    let resp = request
        .send()
        .await
        .map_err(|e| config_err(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        if let Some((_, value)) = cached {
            return Ok(value);
        }
    }
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(config_err(match label {
            Some(label) => format!("key '{key}' with label '{label}' not found"),
            None => format!("key '{key}' (no label) not found"),
        }));
    }
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    #[derive(serde::Deserialize)]
    struct KeyValue {
        value: Option<String>,
        etag: Option<String>,
    }
    let kv: KeyValue = serde_json::from_slice(&bytes)
        .map_err(|e| config_err(format!("unexpected App Configuration response: {e}")))?;
    let value = kv
        .value
        .ok_or_else(|| config_err(format!("key '{key}' has no value")))?;
    if let Some(etag) = kv.etag {
        etags()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (etag, value.clone()));
    }
    Ok(value)
}

/// Reads one item of a Cosmos DB container through the REST API, with a
/// Microsoft Entra token (the account needs data-plane RBAC; account
/// keys are not used). The last ETag is sent, so an unchanged item is a
/// 304.
pub(crate) async fn fetch_cosmos(
    account: &str,
    database: &str,
    container: &str,
    id: &str,
    pk: Option<&str>,
    field: Option<&str>,
) -> Result<String, SignalError> {
    let field = field.unwrap_or("config");
    // AZURE_COSMOS_ENDPOINT, with `{account}` in it, covers sovereign
    // clouds and the emulator.
    let endpoint = match std::env::var("AZURE_COSMOS_ENDPOINT") {
        Ok(template) => {
            let endpoint = template.replace("{account}", account);
            check_endpoint(&endpoint)?;
            endpoint.trim_end_matches('/').to_string()
        }
        Err(_) => format!("https://{account}.documents.azure.com"),
    };
    let scope = format!("{endpoint}/.default");
    let mut url = reqwest::Url::parse(&endpoint).map_err(|e| config_err(e.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| config_err(format!("{endpoint} cannot be a base URL")))?
        .extend(["dbs", database, "colls", container, "docs", id]);

    let token = token(&scope).await?;
    let auth: String =
        url::form_urlencoded::byte_serialize(format!("type=aad&ver=1.0&sig={token}").as_bytes())
            .collect();
    let partition_key = serde_json::json!([pk.unwrap_or(id)]).to_string();
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();

    let cache_id = url.to_string();
    let cached = etags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_id)
        .cloned();
    let mut request = client()?
        .get(url)
        .header("Authorization", auth)
        .header("x-ms-version", "2018-12-31")
        .header("x-ms-date", date)
        .header("x-ms-documentdb-partitionkey", partition_key);
    if let Some((etag, _)) = &cached {
        request = request.header("If-None-Match", etag);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| config_err(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        if let Some((_, value)) = cached {
            return Ok(value);
        }
    }
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(config_err(format!(
            "no item '{id}' with partition key '{}'",
            pk.unwrap_or(id)
        )));
    }
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    let item: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| config_err(format!("unexpected Cosmos DB response: {e}")))?;
    let value = item
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            config_err(format!(
                "item '{id}' has no string property '{field}' holding the config"
            ))
        })?;
    if let Some(etag) = item.get("_etag").and_then(|v| v.as_str()) {
        etags()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(cache_id, (etag.to_string(), value.clone()));
    }
    Ok(value)
}

/// Reads a Key Vault secret, its current version unless `version` is
/// set. `AZURE_KEYVAULT_ENDPOINT`, with `{vault}` in it, covers
/// sovereign clouds and emulators.
pub(crate) async fn fetch_secret(
    vault: &str,
    name: &str,
    version: &str,
) -> Result<String, SignalError> {
    let (endpoint, scope) = match std::env::var("AZURE_KEYVAULT_ENDPOINT") {
        Ok(template) => {
            let endpoint = template.replace("{vault}", vault);
            check_endpoint(&endpoint)?;
            let endpoint = endpoint.trim_end_matches('/').to_string();
            let scope = format!("{endpoint}/.default");
            (endpoint, scope)
        }
        Err(_) => (
            format!("https://{vault}.vault.azure.net"),
            "https://vault.azure.net/.default".to_string(),
        ),
    };
    let mut url = reqwest::Url::parse(&endpoint).map_err(|e| config_err(e.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| config_err(format!("{endpoint} cannot be a base URL")))?
        .push("secrets")
        .push(name);
    if !version.is_empty() {
        url.path_segments_mut()
            .map_err(|_| config_err(format!("{endpoint} cannot be a base URL")))?
            .push(version);
    }
    url.query_pairs_mut().append_pair("api-version", "7.4");
    let resp = client()?
        .get(url)
        .bearer_auth(token(&scope).await?)
        .send()
        .await
        .map_err(|e| config_err(e.to_string()))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    if !status.is_success() {
        return Err(status_err(status, &bytes));
    }
    #[derive(serde::Deserialize)]
    struct Secret {
        value: Option<String>,
    }
    serde_json::from_slice::<Secret>(&bytes)
        .map_err(|e| config_err(format!("unexpected Key Vault response: {e}")))?
        .value
        .ok_or_else(|| config_err(format!("secret '{name}' has no value")))
}
