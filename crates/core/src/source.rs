//! Where `signal.yaml` comes from, and how it is fetched and cached.
//!
//! `$SIGNAL_CONFIG` can name a local path or a remote object:
//!
//! - `/etc/opssignal/signal.yaml` (or any path): read from disk
//! - `https://config.example.com/signal.yaml`: plain GET
//! - `s3://bucket/key`: needs the `s3` cargo feature
//! - `gs://bucket/object`: needs the `gcs` cargo feature
//! - `az://container/blob`: needs the `azure` cargo feature; the storage
//!   account comes from `$AZURE_STORAGE_ACCOUNT_NAME`
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
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

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
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::File(p) => write!(f, "{}", p.display()),
            ConfigSource::Http(url) => f.write_str(url),
            ConfigSource::S3 { bucket, key } => write!(f, "s3://{bucket}/{key}"),
            ConfigSource::Gcs { bucket, object } => write!(f, "gs://{bucket}/{object}"),
            ConfigSource::Azure { container, blob } => write!(f, "az://{container}/{blob}"),
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
                "{raw}: unsupported scheme (use a local path, https://, s3://, gs:// or az://)"
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
        };
        if self.is_remote() && !joined.is_remote() {
            return Err(config_err(format!(
                "{self}: a remote config cannot extend the local file {joined}"
            )));
        }
        Ok(joined)
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
        let (scheme, built_in) = match self {
            ConfigSource::File(_) | ConfigSource::Http(_) => return Ok(()),
            ConfigSource::S3 { .. } => ("s3", cfg!(feature = "s3")),
            ConfigSource::Gcs { .. } => ("gs", cfg!(feature = "gcs")),
            ConfigSource::Azure { .. } => ("az", cfg!(feature = "azure")),
        };
        if built_in || registered_fetcher(scheme).is_some() {
            Ok(())
        } else {
            Err(config_err(format!("{self}: {}", missing_support(scheme))))
        }
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
            ConfigSource::S3 { bucket, key } => fetch_s3(bucket, key).await,
            ConfigSource::Gcs { bucket, object } => fetch_gcs(bucket, object).await,
            ConfigSource::Azure { container, blob } => fetch_azure(container, blob).await,
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

/// Lets object_store run its requests (including credential and token
/// lookups) on the crate's existing reqwest 0.12 client. Config files
/// are small, so bodies are buffered rather than streamed.
#[cfg(feature = "object-store")]
#[derive(Debug)]
struct ReqwestConnector;

#[cfg(feature = "object-store")]
impl object_store::client::HttpConnector for ReqwestConnector {
    fn connect(
        &self,
        options: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        // object_store refuses plain http unless allowed (it allows it
        // for metadata and token endpoints, or when AWS_ALLOW_HTTP and
        // friends are set); keep that rule with our own client.
        let allow_http = options
            .get_config_value(&object_store::ClientConfigKey::AllowHttp)
            .is_some_and(|v| v == "true");
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .https_only(!allow_http)
            .build()
            .map_err(|e| object_store::Error::Generic {
                store: "opssignal",
                source: Box::new(e),
            })?;
        Ok(object_store::client::HttpClient::new(ReqwestService(
            client,
        )))
    }
}

#[cfg(feature = "object-store")]
#[derive(Debug)]
struct ReqwestService(reqwest::Client);

#[cfg(feature = "object-store")]
#[async_trait::async_trait]
impl object_store::client::HttpService for ReqwestService {
    async fn call(
        &self,
        req: object_store::client::HttpRequest,
    ) -> Result<object_store::client::HttpResponse, object_store::client::HttpError> {
        use object_store::client::{HttpError, HttpErrorKind, HttpResponseBody};

        let (parts, body) = req.into_parts();
        let body = match body.as_bytes() {
            Some(bytes) => bytes.clone(),
            None if body.is_empty() => bytes::Bytes::new(),
            None => {
                return Err(HttpError::new_boxed(
                    HttpErrorKind::Request,
                    "streaming request bodies are not supported".into(),
                ))
            }
        };
        let url = reqwest::Url::parse(&parts.uri.to_string())
            .map_err(|e| HttpError::new(HttpErrorKind::Request, e))?;
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = parts.headers;
        *request.body_mut() = Some(body.into());

        let kind = |e: &reqwest::Error| {
            if e.is_timeout() {
                HttpErrorKind::Timeout
            } else if e.is_connect() {
                HttpErrorKind::Connect
            } else {
                HttpErrorKind::Request
            }
        };
        let resp = self
            .0
            .execute(request)
            .await
            .map_err(|e| HttpError::new(kind(&e), e))?;
        let mut out = http::Response::builder().status(resp.status());
        if let Some(headers) = out.headers_mut() {
            *headers = resp.headers().clone();
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| HttpError::new(HttpErrorKind::Decode, e))?;
        out.body(HttpResponseBody::from(bytes))
            .map_err(|e| HttpError::new(HttpErrorKind::Interrupted, e))
    }
}

#[cfg(feature = "object-store")]
async fn fetch_object(
    store: impl object_store::ObjectStore,
    path: &str,
) -> Result<String, SignalError> {
    use object_store::ObjectStoreExt;
    let get = async {
        let bytes = store
            .get(&object_store::path::Path::from(path))
            .await?
            .bytes()
            .await?;
        Ok::<_, object_store::Error>(bytes)
    };
    let bytes = tokio::time::timeout(FETCH_TIMEOUT, get)
        .await
        .map_err(|_| config_err(format!("timed out after {FETCH_TIMEOUT:?}")))?
        .map_err(|e| config_err(e.to_string()))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| config_err("config is not valid UTF-8"))
}

#[cfg(feature = "s3")]
async fn fetch_s3(bucket: &str, key: &str) -> Result<String, SignalError> {
    // from_env picks up AWS_REGION, AWS_ENDPOINT_URL, static keys,
    // web identity (EKS IRSA), ECS task roles and EC2 instance profiles.
    let store = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    fetch_object(store, key).await
}

#[cfg(not(feature = "s3"))]
async fn fetch_s3(bucket: &str, key: &str) -> Result<String, SignalError> {
    fetch_external("s3", format!("s3://{bucket}/{key}")).await
}

#[cfg(feature = "gcs")]
async fn fetch_gcs(bucket: &str, object: &str) -> Result<String, SignalError> {
    // from_env picks up GOOGLE_SERVICE_ACCOUNT / GOOGLE_APPLICATION_CREDENTIALS,
    // gcloud application-default credentials, then the metadata server
    // (GKE workload identity, GCE service accounts).
    let store = object_store::gcp::GoogleCloudStorageBuilder::from_env()
        .with_bucket_name(bucket)
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    fetch_object(store, object).await
}

#[cfg(not(feature = "gcs"))]
async fn fetch_gcs(bucket: &str, object: &str) -> Result<String, SignalError> {
    fetch_external("gs", format!("gs://{bucket}/{object}")).await
}

#[cfg(feature = "azure")]
async fn fetch_azure(container: &str, blob: &str) -> Result<String, SignalError> {
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

#[cfg(not(feature = "azure"))]
async fn fetch_azure(container: &str, blob: &str) -> Result<String, SignalError> {
    fetch_external("az", format!("az://{container}/{blob}")).await
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

/// Registers `fetcher` for `scheme` (`s3`, `gs` or `az`), replacing any
/// earlier one. A scheme compiled into this build ignores it.
pub fn register_fetcher(scheme: &str, fetcher: Arc<dyn ExternalFetcher>) {
    fetchers()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(scheme.to_string(), fetcher);
}

fn registered_fetcher(scheme: &str) -> Option<Arc<dyn ExternalFetcher>> {
    fetchers()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(scheme)
        .cloned()
}

#[cfg_attr(
    all(feature = "s3", feature = "gcs", feature = "azure"),
    allow(dead_code)
)]
async fn fetch_external(scheme: &str, url: String) -> Result<String, SignalError> {
    let fetcher = registered_fetcher(scheme).ok_or_else(|| config_err(missing_support(scheme)))?;
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

fn missing_support(scheme: &str) -> String {
    let (extra, feature) = match scheme {
        "s3" => ("aws", "s3"),
        "gs" => ("gcp", "gcs"),
        _ => ("azure", "azure"),
    };
    format!(
        "this build cannot read {scheme}:// configs; install the {extra} add-on: \
         pip install 'opssignal[{extra}]' for Python, a CLI or C library built with \
         `--features {extra}`, or the `{feature}` cargo feature of opssignal-core"
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

    #[cfg(not(feature = "s3"))]
    #[tokio::test]
    async fn s3_without_the_feature_or_an_add_on_says_how_to_get_it() {
        let source = ConfigSource::parse("s3://b/k").expect("valid source");
        let err = source.check_supported().expect_err("unsupported");
        assert!(err.to_string().contains("opssignal[aws]"));
        assert!(err.to_string().contains("`s3` cargo feature"));
        let err = source.fetch().await.expect_err("must be rejected");
        assert!(err.to_string().contains("opssignal[aws]"));
    }

    #[cfg(not(feature = "gcs"))]
    struct Fixed(&'static str);

    #[cfg(not(feature = "gcs"))]
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

    #[cfg(not(feature = "gcs"))]
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
