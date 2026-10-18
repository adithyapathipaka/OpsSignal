//! Where `signal.yaml` comes from, and how it is fetched and cached.
//!
//! `$SIGNAL_CONFIG` can name a local path or a remote object:
//!
//! - `/etc/opssignal/signal.yaml` (or any path): read from disk
//! - `https://config.example.com/signal.yaml`: plain GET
//! - `s3://bucket/key`: needs the `aws` cargo feature
//! - `gs://bucket/object`: needs the `gcp` cargo feature
//! - `az://container/blob`: needs the `azure` cargo feature; the storage
//!   account comes from `$AZURE_STORAGE_ACCOUNT_NAME`
//! - `appconfig://application/environment/profile`: AWS AppConfig,
//!   needs the `aws` cargo feature
//! - `azappconfig://store/key?label=prod`: Azure App Configuration,
//!   needs the `azure` cargo feature
//! - `dynamodb://table/id`: an Amazon DynamoDB item, needs `aws`
//! - `firestore://project/collection/document`: a Firestore document,
//!   needs `gcp`
//! - `cosmos://account/database/container/id`: an Azure Cosmos DB item,
//!   needs `azure`
//!
//! Cloud fetches use each platform's standard credential chain (IAM
//! role / IRSA / env keys on AWS; workload identity / ADC / service
//! account on GCP; workload identity / managed identity / service
//! principal / SAS on Azure), so OpsSignal adds no keys of its own.
//!
//! A remote config is cached on local disk after every successful
//! fetch, as the raw text *before* `${VAR}` interpolation, so no
//! secret is ever written to the cache. If the source can't be reached
//! or returns a broken file, the last good cached copy is used instead:
//! a bucket outage must never be the reason an alert goes missing.
//!
//! `$SIGNAL_CONFIG` may also list several files, comma separated, and
//! any file may `extends:` others; see [`Config::from_layers`] for how
//! they merge. Each remote file in the chain is cached on its own.

use crate::chain::{self, Mode};
use crate::config::{Config, CONFIG_ENV_VAR, DEFAULT_CONFIG_FILE};
use crate::signal::SignalError;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

pub const REFRESH_ENV_VAR: &str = "SIGNAL_CONFIG_REFRESH_SECS";
pub const CACHE_DIR_ENV_VAR: &str = "SIGNAL_CONFIG_CACHE_DIR";
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(60);
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    File(PathBuf),
    /// `https://`, or `http://` to a loopback host only.
    Http(String),
    S3 {
        bucket: String,
        key: String,
    },
    Gcs {
        bucket: String,
        object: String,
    },
    Azure {
        container: String,
        blob: String,
    },
    /// An AWS AppConfig configuration profile.
    AppConfig {
        application: String,
        environment: String,
        profile: String,
    },
    /// One key-value of an Azure App Configuration store; no `label`
    /// means the unlabelled value.
    AzureAppConfig {
        store: String,
        key: String,
        label: Option<String>,
    },
    /// A DynamoDB item, read by its string partition key (attribute
    /// `key`, default `id`); the YAML is in attribute `value` (default
    /// `config`).
    DynamoDb {
        table: String,
        id: String,
        key: Option<String>,
        value: Option<String>,
    },
    /// A Firestore document (`collection/doc`, possibly nested); the
    /// YAML is in string field `field` (default `config`).
    Firestore {
        project: String,
        document: String,
        database: Option<String>,
        field: Option<String>,
    },
    /// A Cosmos DB item, read by id; its partition key value is `pk`, or
    /// the id itself. The YAML is in string property `field` (default
    /// `config`).
    Cosmos {
        account: String,
        database: String,
        container: String,
        id: String,
        pk: Option<String>,
        field: Option<String>,
    },
}

/// The URL schemes each cloud's add-on serves.
pub const CLOUD_SCHEMES: [(&str, &[&str]); 3] = [
    ("aws", &["s3", "appconfig", "dynamodb"]),
    ("gcp", &["gs", "firestore"]),
    ("azure", &["az", "azappconfig", "cosmos"]),
];

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::File(p) => write!(f, "{}", p.display()),
            ConfigSource::Http(url) => f.write_str(url),
            ConfigSource::S3 { bucket, key } => write!(f, "s3://{bucket}/{key}"),
            ConfigSource::Gcs { bucket, object } => write!(f, "gs://{bucket}/{object}"),
            ConfigSource::Azure { container, blob } => write!(f, "az://{container}/{blob}"),
            ConfigSource::AppConfig {
                application,
                environment,
                profile,
            } => write!(f, "appconfig://{application}/{environment}/{profile}"),
            ConfigSource::AzureAppConfig { store, key, label } => {
                write!(f, "azappconfig://{store}/{key}")?;
                write_query(f, &[("label", label)])
            }
            ConfigSource::DynamoDb {
                table,
                id,
                key,
                value,
            } => {
                write!(f, "dynamodb://{table}/{id}")?;
                write_query(f, &[("key", key), ("value", value)])
            }
            ConfigSource::Firestore {
                project,
                document,
                database,
                field,
            } => {
                write!(f, "firestore://{project}/{document}")?;
                write_query(f, &[("database", database), ("field", field)])
            }
            ConfigSource::Cosmos {
                account,
                database,
                container,
                id,
                pk,
                field,
            } => {
                write!(f, "cosmos://{account}/{database}/{container}/{id}")?;
                write_query(f, &[("pk", pk), ("field", field)])
            }
        }
    }
}

impl ConfigSource {
    /// Parses a `$SIGNAL_CONFIG` value. Anything without a recognised
    /// scheme is a local path.
    pub fn parse(raw: &str) -> Result<Self, SignalError> {
        let raw = raw.trim();
        if let Some(rest) = raw.strip_prefix("s3://") {
            let (bucket, key) = split_bucket(rest, raw)?;
            return Ok(ConfigSource::S3 { bucket, key });
        }
        if let Some(rest) = raw.strip_prefix("gs://") {
            let (bucket, object) = split_bucket(rest, raw)?;
            return Ok(ConfigSource::Gcs { bucket, object });
        }
        if let Some(rest) = raw.strip_prefix("az://") {
            let (container, blob) = split_bucket(rest, raw)?;
            return Ok(ConfigSource::Azure { container, blob });
        }
        if let Some(rest) = raw.strip_prefix("appconfig://") {
            let parts: Vec<&str> = rest.split('/').collect();
            if let [application, environment, profile] = parts[..] {
                if ![application, environment, profile]
                    .iter()
                    .any(|p| p.is_empty())
                {
                    return Ok(ConfigSource::AppConfig {
                        application: application.to_string(),
                        environment: environment.to_string(),
                        profile: profile.to_string(),
                    });
                }
            }
            return Err(config_err(format!(
                "{raw}: expected appconfig://<application>/<environment>/<configuration profile>"
            )));
        }
        if let Some(rest) = raw.strip_prefix("azappconfig://") {
            let (path, [label]) = split_query(rest, raw, ["label"])?;
            let (store, key) = split_bucket(path, raw)?;
            return Ok(ConfigSource::AzureAppConfig { store, key, label });
        }
        if let Some(rest) = raw.strip_prefix("dynamodb://") {
            let (path, [key, value]) = split_query(rest, raw, ["key", "value"])?;
            return match path.split_once('/') {
                Some((table, id)) if !table.is_empty() && !id.is_empty() && !id.contains('/') => {
                    Ok(ConfigSource::DynamoDb {
                        table: table.to_string(),
                        id: id.to_string(),
                        key,
                        value,
                    })
                }
                _ => Err(config_err(format!(
                    "{raw}: expected dynamodb://<table>/<item id>"
                ))),
            };
        }
        if let Some(rest) = raw.strip_prefix("firestore://") {
            let (path, [database, field]) = split_query(rest, raw, ["database", "field"])?;
            let (project, document) = split_bucket(path, raw)?;
            let segments: Vec<&str> = document.split('/').collect();
            if !segments.len().is_multiple_of(2) || segments.iter().any(|s| s.is_empty()) {
                return Err(config_err(format!(
                    "{raw}: expected firestore://<project>/<collection>/<document>"
                )));
            }
            return Ok(ConfigSource::Firestore {
                project,
                document,
                database,
                field,
            });
        }
        if let Some(rest) = raw.strip_prefix("cosmos://") {
            let (path, [pk, field]) = split_query(rest, raw, ["pk", "field"])?;
            let parts: Vec<&str> = path.split('/').collect();
            return match parts[..] {
                [account, database, container, id]
                    if ![account, database, container, id]
                        .iter()
                        .any(|p| p.is_empty()) =>
                {
                    Ok(ConfigSource::Cosmos {
                        account: account.to_string(),
                        database: database.to_string(),
                        container: container.to_string(),
                        id: id.to_string(),
                        pk,
                        field,
                    })
                }
                _ => Err(config_err(format!(
                    "{raw}: expected cosmos://<account>/<database>/<container>/<item id>"
                ))),
            };
        }
        if raw.starts_with("https://") {
            return Ok(ConfigSource::Http(raw.to_string()));
        }
        if let Some(rest) = raw.strip_prefix("http://") {
            // Plain HTTP would let anyone on the path rewrite the routing
            // rules, so it is allowed only for local testing.
            let authority = rest.split('/').next().unwrap_or("");
            let host = match authority.find(']') {
                Some(end) if authority.starts_with('[') => &authority[..=end],
                _ => authority.split(':').next().unwrap_or(""),
            };
            if matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
                return Ok(ConfigSource::Http(raw.to_string()));
            }
            return Err(config_err(format!(
                "{raw}: plain http:// is only allowed for localhost; use https://"
            )));
        }
        if raw.contains("://") {
            return Err(config_err(format!(
                "{raw}: unsupported scheme (use a local path, https://, s3://, gs://, az://, \
                 appconfig://, azappconfig://, dynamodb://, firestore:// or cosmos://)"
            )));
        }
        Ok(ConfigSource::File(PathBuf::from(raw)))
    }

    /// Parses a `$SIGNAL_CONFIG` value that lists one or more files,
    /// comma separated, lowest priority first.
    pub fn parse_list(raw: &str) -> Result<Vec<Self>, SignalError> {
        let sources = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Self::parse)
            .collect::<Result<Vec<_>, _>>()?;
        if sources.is_empty() {
            return Err(config_err(format!("'{raw}' names no config file")));
        }
        Ok(sources)
    }

    /// `$SIGNAL_CONFIG` if set, else `./signal.yaml` if it exists. A
    /// local `$SIGNAL_CONFIG` path that is not a file is an error rather
    /// than a silent fall back to defaults.
    pub fn discover() -> Result<Option<Vec<Self>>, SignalError> {
        if let Some(raw) = std::env::var_os(CONFIG_ENV_VAR) {
            let sources = Self::parse_list(&raw.to_string_lossy())?;
            for source in &sources {
                if let ConfigSource::File(path) = source {
                    if !path.is_file() {
                        return Err(config_err(format!(
                            "${CONFIG_ENV_VAR} points to {}, which is not a file",
                            path.display()
                        )));
                    }
                }
            }
            return Ok(Some(sources));
        }
        let local = PathBuf::from(DEFAULT_CONFIG_FILE);
        Ok(local.is_file().then(|| vec![ConfigSource::File(local)]))
    }

    /// Resolves an `extends` entry of this file. A URL or an absolute
    /// path is taken as is; anything else is relative to this file's
    /// own location, in the same bucket, container or directory.
    ///
    /// A remote file may only extend other remote files: a config
    /// served over the network must not be able to read local paths.
    pub fn join(&self, reference: &str) -> Result<Self, SignalError> {
        let reference = reference.trim();
        let absolute = reference.contains("://") || Path::new(reference).is_absolute();
        let joined = match self {
            _ if absolute => Self::parse(reference)?,
            ConfigSource::File(path) => ConfigSource::File(normalize(
                &path
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(reference),
            )),
            ConfigSource::Http(url) => {
                let joined = reqwest::Url::parse(url)
                    .and_then(|base| base.join(reference))
                    .map_err(|e| {
                        config_err(format!("{self}: cannot resolve '{reference}': {e}"))
                    })?;
                Self::parse(joined.as_str())?
            }
            ConfigSource::S3 { bucket, key } => ConfigSource::S3 {
                bucket: bucket.clone(),
                key: self.join_key(key, reference)?,
            },
            ConfigSource::Gcs { bucket, object } => ConfigSource::Gcs {
                bucket: bucket.clone(),
                object: self.join_key(object, reference)?,
            },
            ConfigSource::Azure { container, blob } => ConfigSource::Azure {
                container: container.clone(),
                blob: self.join_key(blob, reference)?,
            },
            // Profiles have flat names: a relative entry names another
            // profile of the same application and environment.
            ConfigSource::AppConfig {
                application,
                environment,
                ..
            } => {
                if reference.is_empty() || reference.contains('/') {
                    return Err(config_err(format!(
                        "{self}: '{reference}' is not a profile name; use a full appconfig:// URL"
                    )));
                }
                ConfigSource::AppConfig {
                    application: application.clone(),
                    environment: environment.clone(),
                    profile: reference.to_string(),
                }
            }
            // Keys are often path-like (`opssignal/teams/payments`), so
            // they resolve like paths, keeping the label.
            ConfigSource::AzureAppConfig { store, key, label } => ConfigSource::AzureAppConfig {
                store: store.clone(),
                key: self.join_key(key, reference)?,
                label: label.clone(),
            },
            // Another item of the same table, read the same way.
            ConfigSource::DynamoDb {
                table, key, value, ..
            } => ConfigSource::DynamoDb {
                table: table.clone(),
                id: self.flat_name(reference)?,
                key: key.clone(),
                value: value.clone(),
            },
            // Document paths resolve like paths, in the same database.
            ConfigSource::Firestore {
                project,
                document,
                database,
                field,
            } => {
                let document = self.join_key(document, reference)?;
                if !document.split('/').count().is_multiple_of(2) {
                    return Err(config_err(format!(
                        "{self}: '{reference}' is a collection, not a document"
                    )));
                }
                ConfigSource::Firestore {
                    project: project.clone(),
                    document,
                    database: database.clone(),
                    field: field.clone(),
                }
            }
            // Another item of the same container. Its partition key is
            // assumed to be its id; use a full URL with ?pk= otherwise.
            ConfigSource::Cosmos {
                account,
                database,
                container,
                field,
                ..
            } => ConfigSource::Cosmos {
                account: account.clone(),
                database: database.clone(),
                container: container.clone(),
                id: self.flat_name(reference)?,
                pk: None,
                field: field.clone(),
            },
        };
        if self.is_remote() && !joined.is_remote() {
            return Err(config_err(format!(
                "{self}: a remote config cannot extend the local file {joined}"
            )));
        }
        Ok(joined)
    }

    /// A relative `extends` entry that names a sibling item.
    fn flat_name(&self, reference: &str) -> Result<String, SignalError> {
        if reference.is_empty() || reference.contains('/') || reference.contains('?') {
            return Err(config_err(format!(
                "{self}: '{reference}' is not an item id; use a full {}:// URL",
                self.scheme()
            )));
        }
        Ok(reference.to_string())
    }

    /// `reference` resolved against the "directory" of object `key`.
    fn join_key(&self, key: &str, reference: &str) -> Result<String, SignalError> {
        let mut parts: Vec<&str> = key.split('/').collect();
        parts.pop();
        for segment in reference.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(config_err(format!(
                            "{self}: '{reference}' points outside the bucket"
                        )));
                    }
                }
                name => parts.push(name),
            }
        }
        if parts.is_empty() {
            return Err(config_err(format!(
                "{self}: '{reference}' does not name a file"
            )));
        }
        Ok(parts.join("/"))
    }

    /// Whether this build can read the source at all, either natively
    /// or through a registered [`ExternalFetcher`]. A missing one is an
    /// install problem, not an outage, so it fails loudly instead of
    /// falling back.
    pub fn check_supported(&self) -> Result<(), SignalError> {
        let Some(cloud) = self.cloud() else {
            return Ok(());
        };
        let built_in = (cloud == "aws" && cfg!(feature = "aws"))
            || (cloud == "gcp" && cfg!(feature = "gcp"))
            || (cloud == "azure" && cfg!(feature = "azure"));
        if built_in || registered_fetcher(self.scheme()).is_some() {
            Ok(())
        } else {
            Err(config_err(format!("{self}: {}", missing_support(self))))
        }
    }

    /// The URL scheme, or `file` for a local path.
    pub fn scheme(&self) -> &'static str {
        match self {
            ConfigSource::File(_) => "file",
            ConfigSource::Http(url) if url.starts_with("http://") => "http",
            ConfigSource::Http(_) => "https",
            ConfigSource::S3 { .. } => "s3",
            ConfigSource::Gcs { .. } => "gs",
            ConfigSource::Azure { .. } => "az",
            ConfigSource::AppConfig { .. } => "appconfig",
            ConfigSource::AzureAppConfig { .. } => "azappconfig",
            ConfigSource::DynamoDb { .. } => "dynamodb",
            ConfigSource::Firestore { .. } => "firestore",
            ConfigSource::Cosmos { .. } => "cosmos",
        }
    }

    /// The cloud whose add-on reads this source, if any.
    pub fn cloud(&self) -> Option<&'static str> {
        let scheme = self.scheme();
        CLOUD_SCHEMES
            .iter()
            .find(|(_, schemes)| schemes.contains(&scheme))
            .map(|(cloud, _)| *cloud)
    }

    pub fn is_remote(&self) -> bool {
        !matches!(self, ConfigSource::File(_))
    }

    /// Reads the raw (uninterpolated) config text from the source.
    pub async fn fetch(&self) -> Result<String, SignalError> {
        self.fetch_unprefixed()
            .await
            .map_err(|e| config_err(format!("{self}: {}", message(&e))))
    }

    /// [`ConfigSource::fetch`] without the source named in the error,
    /// for add-ons whose caller adds it.
    async fn fetch_unprefixed(&self) -> Result<String, SignalError> {
        match self {
            ConfigSource::File(path) => tokio::fs::read_to_string(path)
                .await
                .map_err(|e| config_err(format!("cannot read {}: {e}", path.display()))),
            ConfigSource::Http(url) => fetch_http(url).await,
            _ => self.fetch_cloud().await,
        }
    }

    /// A cloud source, from this build or from its add-on.
    async fn fetch_cloud(&self) -> Result<String, SignalError> {
        match self {
            #[cfg(feature = "aws")]
            ConfigSource::S3 { bucket, key } => crate::cloud::aws::fetch_s3(bucket, key).await,
            #[cfg(feature = "aws")]
            ConfigSource::AppConfig {
                application,
                environment,
                profile,
            } => crate::cloud::aws::fetch_appconfig(application, environment, profile).await,
            #[cfg(feature = "aws")]
            ConfigSource::DynamoDb {
                table,
                id,
                key,
                value,
            } => {
                crate::cloud::aws::fetch_dynamodb(table, id, key.as_deref(), value.as_deref()).await
            }
            #[cfg(feature = "gcp")]
            ConfigSource::Gcs { bucket, object } => {
                crate::cloud::gcp::fetch_gcs(bucket, object).await
            }
            #[cfg(feature = "gcp")]
            ConfigSource::Firestore {
                project,
                document,
                database,
                field,
            } => {
                crate::cloud::gcp::fetch_firestore(
                    project,
                    document,
                    database.as_deref(),
                    field.as_deref(),
                )
                .await
            }
            #[cfg(feature = "azure")]
            ConfigSource::Cosmos {
                account,
                database,
                container,
                id,
                pk,
                field,
            } => {
                crate::cloud::azure::fetch_cosmos(
                    account,
                    database,
                    container,
                    id,
                    pk.as_deref(),
                    field.as_deref(),
                )
                .await
            }
            #[cfg(feature = "azure")]
            ConfigSource::Azure { container, blob } => {
                crate::cloud::azure::fetch_blob(container, blob).await
            }
            #[cfg(feature = "azure")]
            ConfigSource::AzureAppConfig { store, key, label } => {
                crate::cloud::azure::fetch_app_config(store, key, label.as_deref()).await
            }
            _ => fetch_external(self.scheme(), self.to_string()).await,
        }
    }
}

/// Drops `.` and folds `..` into its parent without touching the disk,
/// so two teams reaching one shared file by different relative paths
/// load it once.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Splits `path?a=1&b=2` into the path and the values of `allowed`, in
/// that order. Unknown or repeated parameters are errors, so a typo
/// can't silently read the wrong item.
fn split_query<'a, const N: usize>(
    rest: &'a str,
    raw: &str,
    allowed: [&str; N],
) -> Result<(&'a str, [Option<String>; N]), SignalError> {
    let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
    let Some((path, query)) = rest.split_once('?') else {
        return Ok((rest, values));
    };
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let slot = allowed.iter().position(|a| *a == name);
        match slot {
            Some(i) if values[i].is_none() && !value.is_empty() => {
                values[i] = Some(value.to_string());
            }
            _ => {
                let allowed: Vec<String> = allowed.iter().map(|a| format!("{a}=")).collect();
                return Err(config_err(format!(
                    "{raw}: unexpected '{pair}' (allowed once each, with a value: {})",
                    allowed.join(", ")
                )));
            }
        }
    }
    Ok((path, values))
}

fn write_query(f: &mut fmt::Formatter<'_>, pairs: &[(&str, &Option<String>)]) -> fmt::Result {
    let mut sep = '?';
    for (name, value) in pairs {
        if let Some(value) = value {
            write!(f, "{sep}{name}={value}")?;
            sep = '&';
        }
    }
    Ok(())
}

fn split_bucket(rest: &str, raw: &str) -> Result<(String, String), SignalError> {
    match rest.split_once('/') {
        Some((bucket, key)) if !bucket.is_empty() && !key.is_empty() => {
            Ok((bucket.to_string(), key.to_string()))
        }
        _ => Err(config_err(format!(
            "{raw}: expected <scheme>://<bucket>/<path to signal.yaml>"
        ))),
    }
}

async fn fetch_http(url: &str) -> Result<String, SignalError> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| config_err(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(config_err(format!("server returned {}", resp.status())));
    }
    resp.text().await.map_err(|e| config_err(e.to_string()))
}

/// Reads one URL scheme for a build that was compiled without it.
///
/// Language bindings register these at runtime from add-on packages
/// (the Python wheel ships local and HTTPS support only, and
/// `pip install 'opssignal[aws]'` adds an S3 fetcher), so the base
/// package stays small. Implementations may block; they run on their
/// own thread and return the raw, uninterpolated file text.
pub trait ExternalFetcher: Send + Sync {
    fn fetch(&self, url: &str) -> Result<String, String>;
}

type FetcherRegistry = RwLock<HashMap<String, Arc<dyn ExternalFetcher>>>;

fn fetchers() -> &'static FetcherRegistry {
    static FETCHERS: OnceLock<FetcherRegistry> = OnceLock::new();
    FETCHERS.get_or_init(Default::default)
}

/// Registers `fetcher` for one URL scheme (such as `s3`), replacing any
/// earlier one. A scheme compiled into this build ignores it.
pub fn register_fetcher(scheme: &str, fetcher: Arc<dyn ExternalFetcher>) {
    fetchers()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(scheme.to_string(), fetcher);
}

/// Registers `fetcher` for every scheme of `cloud` (`aws`, `gcp` or
/// `azure`; see [`CLOUD_SCHEMES`]). This is what an add-on package does.
pub fn register_cloud_fetcher(cloud: &str, fetcher: Arc<dyn ExternalFetcher>) {
    for (name, schemes) in CLOUD_SCHEMES {
        if name == cloud {
            for scheme in schemes {
                register_fetcher(scheme, Arc::clone(&fetcher));
            }
        }
    }
}

fn registered_fetcher(scheme: &str) -> Option<Arc<dyn ExternalFetcher>> {
    fetchers()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(scheme)
        .cloned()
}

async fn fetch_external(scheme: &str, url: String) -> Result<String, SignalError> {
    let fetcher = registered_fetcher(scheme).ok_or_else(|| {
        config_err(ConfigSource::parse(&url).map_or_else(|e| message(&e), |s| missing_support(&s)))
    })?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A plain thread rather than spawn_blocking: the fetcher may start
    // its own runtime, which tokio forbids on its blocking pool.
    std::thread::Builder::new()
        .name("opssignal-fetch".into())
        .spawn(move || {
            let _ = tx.send(fetcher.fetch(&url));
        })
        .map_err(|e| config_err(format!("cannot start fetch thread: {e}")))?;
    // The fetcher applies FETCH_TIMEOUT itself; this only guards
    // against one that never returns.
    match tokio::time::timeout(FETCH_TIMEOUT * 2, rx).await {
        Ok(Ok(result)) => result.map_err(config_err),
        Ok(Err(_)) => Err(config_err("the add-on fetcher failed without an answer")),
        Err(_) => Err(config_err(format!(
            "timed out after {:?}",
            FETCH_TIMEOUT * 2
        ))),
    }
}

fn missing_support(source: &ConfigSource) -> String {
    let scheme = source.scheme();
    let cloud = source.cloud().unwrap_or("aws");
    format!(
        "this build cannot read {scheme}:// configs; install the {cloud} add-on: \
         pip install 'opssignal[{cloud}]' for Python, or a CLI or C library built with \
         `--features {cloud}` (the `{cloud}` cargo feature of opssignal-core)"
    )
}

/// Fetches the raw text at `url` on a throwaway runtime, with errors
/// in the form an [`ExternalFetcher`] returns them. This is what add-on
/// packages call to serve a request.
pub fn fetch_blocking(url: &str) -> Result<String, String> {
    let fetched = ConfigSource::parse(url).and_then(|source| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| SignalError::Internal(format!("failed to start runtime: {e}")))?
            .block_on(source.fetch_unprefixed())
    });
    fetched.map_err(|e| message(&e))
}

/// How a client loads and refreshes its config.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// How often to re-read the source and apply changes. `None` loads
    /// once and never reloads.
    pub refresh: Option<Duration>,
    /// Where remote configs are cached. `None` disables the cache.
    pub cache_dir: Option<PathBuf>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            refresh: Some(DEFAULT_REFRESH),
            cache_dir: default_cache_dir(),
        }
    }
}

impl LoadOptions {
    /// Defaults, overridden by `$SIGNAL_CONFIG_REFRESH_SECS` (0 turns
    /// reloading off) and `$SIGNAL_CONFIG_CACHE_DIR`.
    pub fn from_env() -> Result<Self, SignalError> {
        let mut opts = LoadOptions::default();
        if let Ok(raw) = std::env::var(REFRESH_ENV_VAR) {
            let secs: u64 = raw.trim().parse().map_err(|_| {
                config_err(format!(
                    "${REFRESH_ENV_VAR} must be a whole number of seconds, got '{raw}'"
                ))
            })?;
            opts.refresh = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(dir) = std::env::var_os(CACHE_DIR_ENV_VAR) {
            opts.cache_dir = Some(PathBuf::from(dir));
        }
        Ok(opts)
    }

    /// Load once, no reloading. What a one-shot CLI call wants.
    pub fn once() -> Self {
        LoadOptions {
            refresh: None,
            ..LoadOptions::default()
        }
    }

    /// Where the cached copy of `source` lives, if it is remote and
    /// caching is on.
    pub fn cache_path(&self, source: &ConfigSource) -> Option<PathBuf> {
        if !source.is_remote() {
            return None;
        }
        let digest = Sha256::digest(source.to_string().as_bytes());
        let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        self.cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("signal-{name}.yaml")))
    }
}

fn default_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    Some(base.join("opssignal"))
}

/// The config a client starts with, plus the raw text of every file it
/// was built from so the reloader can tell when one has changed.
pub(crate) struct Loaded {
    pub config: Config,
    pub texts: Option<Texts>,
}

/// `(source, raw text)` for each file of a chain, in merge order.
pub(crate) type Texts = Vec<(String, String)>;

/// Initial load. A local file must exist and parse, as before. A remote
/// file falls back to its cached copy; if the chain still can't be
/// built, the client starts on the zero-config default, logging why, so
/// that startup never fails on a network blip.
pub(crate) async fn load_initial(
    roots: &[ConfigSource],
    opts: &LoadOptions,
) -> Result<Loaded, SignalError> {
    for root in roots {
        root.check_supported()?;
    }
    let built = match chain::walk(roots, Mode::Startup(opts)).await {
        Ok(files) => chain::merge(&files).map(|config| (config, files)),
        Err(e) => Err(e),
    };
    let built = match built {
        Ok(built) => Ok(built),
        // Every file fetched, but together they don't make a valid
        // config (say a remote team file now routes to a provider
        // nobody configures). The cached copies did, last time.
        Err(e) if !e.fatal && opts.cache_dir.is_some() => {
            match chain::walk(roots, Mode::CacheOnly(opts)).await {
                Ok(files) => match chain::merge(&files) {
                    Ok(config) => {
                        tracing::warn!("{}; using the cached copies", e.err);
                        Ok((config, files))
                    }
                    Err(_) => Err(e),
                },
                Err(_) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    match built {
        Ok((config, files)) => {
            for (fetched, _) in &files {
                if let Some(path) = opts.cache_path(&fetched.source) {
                    write_cache(&path, &fetched.text);
                }
            }
            Ok(Loaded {
                config,
                texts: Some(chain::texts(&files)),
            })
        }
        Err(e) if e.fatal => Err(e.err),
        Err(e) => {
            tracing::error!(
                "{}; no usable cached copy, starting with the default config (console only)",
                e.err
            );
            Ok(Loaded {
                config: Config::default(),
                texts: None,
            })
        }
    }
}

/// [`load_initial`] on a throwaway runtime, for callers that are not
/// async and have no client runtime yet.
pub(crate) fn load_blocking(
    roots: &[ConfigSource],
    opts: &LoadOptions,
) -> Result<Loaded, SignalError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SignalError::Internal(format!("failed to start runtime: {e}")))?
        .block_on(load_initial(roots, opts))
}

/// Write-then-rename so a crash mid-write never leaves a truncated
/// cache behind. Failure only costs the fallback, so it is logged, not
/// returned.
pub(crate) fn write_cache(path: &Path, text: &str) {
    let result = (|| {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        tracing::warn!("cannot write config cache {}: {e}", path.display());
    }
}

fn config_err(msg: impl Into<String>) -> SignalError {
    SignalError::Config(msg.into())
}

/// The message of a config error, without the "config error:" prefix.
pub(crate) fn message(e: &SignalError) -> String {
    match e {
        SignalError::Config(msg) => msg.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_scheme() {
        assert_eq!(
            ConfigSource::parse("s3://ops-config/prod/signal.yaml").expect("valid source"),
            ConfigSource::S3 {
                bucket: "ops-config".into(),
                key: "prod/signal.yaml".into()
            }
        );
        assert_eq!(
            ConfigSource::parse("gs://ops-config/signal.yaml").expect("valid source"),
            ConfigSource::Gcs {
                bucket: "ops-config".into(),
                object: "signal.yaml".into()
            }
        );
        assert_eq!(
            ConfigSource::parse("az://ops-config/prod/signal.yaml").expect("valid source"),
            ConfigSource::Azure {
                container: "ops-config".into(),
                blob: "prod/signal.yaml".into()
            }
        );
        assert_eq!(
            ConfigSource::parse("https://cfg.example.test/signal.yaml").expect("valid source"),
            ConfigSource::Http("https://cfg.example.test/signal.yaml".into())
        );
        assert_eq!(
            ConfigSource::parse("/etc/opssignal/signal.yaml").expect("valid source"),
            ConfigSource::File("/etc/opssignal/signal.yaml".into())
        );
    }

    #[test]
    fn plain_http_is_only_allowed_for_loopback() {
        assert!(ConfigSource::parse("http://127.0.0.1:8080/signal.yaml").is_ok());
        assert!(ConfigSource::parse("http://localhost/signal.yaml").is_ok());
        let err = ConfigSource::parse("http://cfg.example.test/signal.yaml")
            .expect_err("must be rejected");
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn bucket_urls_need_a_bucket_and_a_key() {
        assert!(ConfigSource::parse("s3://bucket-only").is_err());
        assert!(ConfigSource::parse("s3://bucket/").is_err());
        assert!(ConfigSource::parse("gs:///signal.yaml").is_err());
        assert!(ConfigSource::parse("az://container-only").is_err());
    }

    #[test]
    fn unknown_schemes_are_rejected() {
        let err = ConfigSource::parse("ftp://host/signal.yaml").expect_err("must be rejected");
        assert!(err.to_string().contains("unsupported scheme"));
    }

    #[test]
    fn display_round_trips_through_parse() {
        for raw in [
            "s3://b/k/signal.yaml",
            "gs://b/signal.yaml",
            "az://c/signal.yaml",
            "https://x.test/a",
        ] {
            assert_eq!(
                ConfigSource::parse(raw).expect("valid source").to_string(),
                raw
            );
        }
    }

    #[test]
    fn extends_resolves_against_the_parent_location() {
        let join = |base: &str, reference: &str| {
            ConfigSource::parse(base)
                .expect("valid source")
                .join(reference)
                .map(|s| s.to_string())
        };
        assert_eq!(
            join("s3://b/teams/payments.yaml", "../default.yaml").expect("joined"),
            "s3://b/default.yaml"
        );
        assert_eq!(
            join("gs://b/teams/a.yaml", "./shared/db.yaml").expect("joined"),
            "gs://b/teams/shared/db.yaml"
        );
        assert_eq!(
            join("az://c/a.yaml", "s3://other/x.yaml").expect("joined"),
            "s3://other/x.yaml"
        );
        assert_eq!(
            join("https://cfg.test/teams/a.yaml", "../default.yaml").expect("joined"),
            "https://cfg.test/default.yaml"
        );
        assert_eq!(
            join("/etc/opssignal/teams/a.yaml", "../default.yaml").expect("joined"),
            "/etc/opssignal/default.yaml"
        );
        assert_eq!(join("a.yaml", "../x.yaml").expect("joined"), "../x.yaml");
        assert!(join("s3://b/a.yaml", "../../x.yaml").is_err());
        assert!(join("s3://b/a.yaml", "/etc/passwd").is_err());
        assert!(join("https://cfg.test/a.yaml", "/etc/passwd").is_err());
    }

    #[test]
    fn parses_managed_config_sources() {
        assert_eq!(
            ConfigSource::parse("appconfig://opssignal/prod/payments").expect("valid source"),
            ConfigSource::AppConfig {
                application: "opssignal".into(),
                environment: "prod".into(),
                profile: "payments".into()
            }
        );
        assert_eq!(
            ConfigSource::parse("azappconfig://ops/opssignal/teams/a?label=prod")
                .expect("valid source"),
            ConfigSource::AzureAppConfig {
                store: "ops".into(),
                key: "opssignal/teams/a".into(),
                label: Some("prod".into())
            }
        );
        for bad in [
            "appconfig://app/env",
            "appconfig://app//profile",
            "appconfig://a/b/c/d",
            "azappconfig://store-only",
            "azappconfig://s/k?x=1",
            "azappconfig://s/k?label=",
        ] {
            assert!(ConfigSource::parse(bad).is_err(), "{bad}");
        }
        for raw in [
            "appconfig://a/b/c",
            "azappconfig://s/k/x",
            "azappconfig://s/k?label=dev",
        ] {
            assert_eq!(ConfigSource::parse(raw).expect("valid").to_string(), raw);
        }
        assert_eq!(
            ConfigSource::parse("appconfig://a/b/c")
                .expect("valid")
                .cloud(),
            Some("aws")
        );
        assert_eq!(
            ConfigSource::parse("azappconfig://s/k")
                .expect("valid")
                .cloud(),
            Some("azure")
        );
    }

    #[test]
    fn managed_config_extends_stay_in_the_same_store() {
        let app = ConfigSource::parse("appconfig://ops/prod/payments").expect("valid");
        assert_eq!(
            app.join("default").expect("joined").to_string(),
            "appconfig://ops/prod/default"
        );
        assert!(app.join("../default").is_err());
        let kv =
            ConfigSource::parse("azappconfig://ops/opssignal/teams/a?label=prod").expect("valid");
        assert_eq!(
            kv.join("../default").expect("joined").to_string(),
            "azappconfig://ops/opssignal/default?label=prod"
        );
    }

    #[test]
    fn parses_database_sources() {
        assert_eq!(
            ConfigSource::parse("dynamodb://cfg/payments?key=team&value=yaml")
                .expect("valid source"),
            ConfigSource::DynamoDb {
                table: "cfg".into(),
                id: "payments".into(),
                key: Some("team".into()),
                value: Some("yaml".into())
            }
        );
        assert_eq!(
            ConfigSource::parse("firestore://proj/teams/a/configs/main").expect("valid source"),
            ConfigSource::Firestore {
                project: "proj".into(),
                document: "teams/a/configs/main".into(),
                database: None,
                field: None
            }
        );
        assert_eq!(
            ConfigSource::parse("cosmos://acct/db/coll/payments?pk=team-a").expect("valid"),
            ConfigSource::Cosmos {
                account: "acct".into(),
                database: "db".into(),
                container: "coll".into(),
                id: "payments".into(),
                pk: Some("team-a".into()),
                field: None
            }
        );
        for bad in [
            "dynamodb://table-only",
            "dynamodb://t/a/b",
            "dynamodb://t/a?key=",
            "dynamodb://t/a?colour=red",
            "dynamodb://t/a?key=x&key=y",
            "firestore://proj/collection-only",
            "firestore://proj/a//b",
            "cosmos://acct/db/coll",
            "cosmos://acct/db/coll/id/extra",
        ] {
            assert!(ConfigSource::parse(bad).is_err(), "{bad}");
        }
        for raw in [
            "dynamodb://t/a",
            "dynamodb://t/a?key=k&value=v",
            "firestore://p/c/d?database=x&field=f",
            "cosmos://a/d/c/i?pk=p&field=f",
        ] {
            assert_eq!(ConfigSource::parse(raw).expect("valid").to_string(), raw);
        }
        for (raw, cloud) in [
            ("dynamodb://t/a", "aws"),
            ("firestore://p/c/d", "gcp"),
            ("cosmos://a/d/c/i", "azure"),
        ] {
            assert_eq!(
                ConfigSource::parse(raw).expect("valid").cloud(),
                Some(cloud)
            );
        }
    }

    #[test]
    fn database_extends_stay_in_the_same_table() {
        let join = |base: &str, reference: &str| {
            ConfigSource::parse(base)
                .expect("valid source")
                .join(reference)
                .map(|s| s.to_string())
        };
        assert_eq!(
            join("dynamodb://cfg/payments?key=team", "default").expect("joined"),
            "dynamodb://cfg/default?key=team"
        );
        assert!(join("dynamodb://cfg/payments", "a/b").is_err());
        assert_eq!(
            join("firestore://p/opssignal/payments?database=x", "default").expect("joined"),
            "firestore://p/opssignal/default?database=x"
        );
        assert_eq!(
            join("firestore://p/teams/a/configs/main", "../../b/configs/main").expect("joined"),
            "firestore://p/teams/b/configs/main"
        );
        assert!(join("firestore://p/teams/a/configs/main", "../x").is_err());
        assert_eq!(
            join("cosmos://a/d/c/payments?pk=team-a&field=f", "default").expect("joined"),
            "cosmos://a/d/c/default?field=f"
        );
    }

    #[test]
    fn a_config_list_is_comma_separated() {
        let list = ConfigSource::parse_list(" a.yaml, s3://b/k.yaml ,").expect("valid list");
        assert_eq!(
            list,
            [
                ConfigSource::File("a.yaml".into()),
                ConfigSource::S3 {
                    bucket: "b".into(),
                    key: "k.yaml".into()
                }
            ]
        );
        assert!(ConfigSource::parse_list(" , ").is_err());
    }

    #[test]
    fn cache_path_is_per_source_and_only_for_remote_sources() {
        let opts = LoadOptions {
            refresh: None,
            cache_dir: Some(PathBuf::from("/cache")),
        };
        let a = opts.cache_path(&ConfigSource::parse("s3://b/a.yaml").expect("valid source"));
        let b = opts.cache_path(&ConfigSource::parse("s3://b/b.yaml").expect("valid source"));
        assert!(a.is_some() && b.is_some() && a != b);
        assert!(a.expect("valid source").starts_with("/cache"));
        assert_eq!(
            opts.cache_path(&ConfigSource::parse("./signal.yaml").expect("valid source")),
            None
        );
    }

    #[cfg(not(feature = "aws"))]
    #[tokio::test]
    async fn s3_without_the_feature_or_an_add_on_says_how_to_get_it() {
        let source = ConfigSource::parse("s3://b/k").expect("valid source");
        let err = source.check_supported().expect_err("unsupported");
        assert!(err.to_string().contains("opssignal[aws]"));
        assert!(err.to_string().contains("`aws` cargo feature"));
        let err = source.fetch().await.expect_err("must be rejected");
        assert!(err.to_string().contains("opssignal[aws]"));
    }

    #[cfg(not(feature = "gcp"))]
    struct Fixed(&'static str);

    #[cfg(not(feature = "gcp"))]
    impl ExternalFetcher for Fixed {
        fn fetch(&self, url: &str) -> Result<String, String> {
            Ok(format!("{}{url}", self.0))
        }
    }

    #[cfg(not(feature = "azure"))]
    struct Panics;

    #[cfg(not(feature = "azure"))]
    impl ExternalFetcher for Panics {
        fn fetch(&self, _url: &str) -> Result<String, String> {
            panic!("add-on bug")
        }
    }

    #[cfg(not(feature = "gcp"))]
    #[tokio::test]
    async fn a_registered_add_on_serves_its_scheme() {
        let source = ConfigSource::parse("gs://b/k.yaml").expect("valid source");
        assert!(source.check_supported().is_err());

        register_fetcher("gs", Arc::new(Fixed("fetched ")));

        assert!(source.check_supported().is_ok());
        assert_eq!(
            source.fetch().await.expect("fetched"),
            "fetched gs://b/k.yaml"
        );
    }

    #[cfg(not(feature = "azure"))]
    #[tokio::test]
    async fn a_panicking_add_on_is_a_fetch_error() {
        register_fetcher("az", Arc::new(Panics));
        let err = ConfigSource::parse("az://c/k")
            .expect("valid source")
            .fetch()
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("without an answer"));
    }
}
