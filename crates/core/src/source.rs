//! Where `signal.yaml` comes from, and how it is fetched and cached.
//!
//! `$SIGNAL_CONFIG` can name a local path or a remote object:
//!
//! - `/etc/opssignal/signal.yaml` (or any path): read from disk
//! - `https://config.example.com/signal.yaml`: plain GET
//! - `s3://bucket/key`: needs the `s3` cargo feature
//! - `gs://bucket/object`: needs the `gcs` cargo feature
//!
//! Cloud fetches use each platform's standard credential chain (IAM
//! role / IRSA / env keys on AWS; workload identity / ADC / service
//! account on GCP), so OpsSignal adds no keys of its own.
//!
//! A remote config is cached on local disk after every successful
//! fetch, as the raw text *before* `${VAR}` interpolation, so no
//! secret is ever written to the cache. If the source can't be reached
//! or returns a broken file, the last good cached copy is used instead:
//! a bucket outage must never be the reason an alert goes missing.

use crate::config::{Config, CONFIG_ENV_VAR, DEFAULT_CONFIG_FILE};
use crate::signal::SignalError;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
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
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::File(p) => write!(f, "{}", p.display()),
            ConfigSource::Http(url) => f.write_str(url),
            ConfigSource::S3 { bucket, key } => write!(f, "s3://{bucket}/{key}"),
            ConfigSource::Gcs { bucket, object } => write!(f, "gs://{bucket}/{object}"),
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
                "{raw}: unsupported scheme (use a local path, https://, s3:// or gs://)"
            )));
        }
        Ok(ConfigSource::File(PathBuf::from(raw)))
    }

    /// `$SIGNAL_CONFIG` if set, else `./signal.yaml` if it exists. A
    /// local `$SIGNAL_CONFIG` path that is not a file is an error rather
    /// than a silent fall back to defaults.
    pub fn discover() -> Result<Option<Self>, SignalError> {
        if let Some(raw) = std::env::var_os(CONFIG_ENV_VAR) {
            let source = Self::parse(&raw.to_string_lossy())?;
            if let ConfigSource::File(path) = &source {
                if !path.is_file() {
                    return Err(config_err(format!(
                        "${CONFIG_ENV_VAR} points to {}, which is not a file",
                        path.display()
                    )));
                }
            }
            return Ok(Some(source));
        }
        let local = PathBuf::from(DEFAULT_CONFIG_FILE);
        Ok(local.is_file().then_some(ConfigSource::File(local)))
    }

    pub fn is_remote(&self) -> bool {
        !matches!(self, ConfigSource::File(_))
    }

    /// Reads the raw (uninterpolated) config text from the source.
    pub async fn fetch(&self) -> Result<String, SignalError> {
        match self {
            ConfigSource::File(path) => tokio::fs::read_to_string(path)
                .await
                .map_err(|e| config_err(format!("cannot read {}: {e}", path.display()))),
            ConfigSource::Http(url) => fetch_http(url).await,
            ConfigSource::S3 { bucket, key } => fetch_s3(bucket, key).await,
            ConfigSource::Gcs { bucket, object } => fetch_gcs(bucket, object).await,
        }
        .map_err(|e| config_err(format!("{self}: {}", strip_prefix(&e))))
    }
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
        _options: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
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
async fn fetch_s3(_bucket: &str, _key: &str) -> Result<String, SignalError> {
    Err(config_err(
        "this build has no S3 support (enable the `s3` cargo feature of opssignal-core)",
    ))
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
async fn fetch_gcs(_bucket: &str, _object: &str) -> Result<String, SignalError> {
    Err(config_err(
        "this build has no GCS support (enable the `gcs` cargo feature of opssignal-core)",
    ))
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

/// The config a client starts with, plus the raw text it came from so
/// the reloader can tell when the source has changed.
pub(crate) struct Loaded {
    pub config: Config,
    pub text: Option<String>,
}

/// Initial load. A local file must exist and parse, as before. A remote
/// source falls back to the cached copy, then to the zero-config
/// default, logging why, so that startup never fails on a network blip.
pub(crate) async fn load_initial(
    source: &ConfigSource,
    opts: &LoadOptions,
) -> Result<Loaded, SignalError> {
    if !source.is_remote() {
        let text = source.fetch().await?;
        let config = parse(source, &text)?;
        return Ok(Loaded {
            config,
            text: Some(text),
        });
    }

    let cache = opts.cache_path(source);
    let fresh = match source.fetch().await {
        Ok(text) => parse(source, &text).map(|config| (config, text)),
        Err(e) => Err(e),
    };
    match fresh {
        Ok((config, text)) => {
            if let Some(path) = &cache {
                write_cache(path, &text);
            }
            Ok(Loaded {
                config,
                text: Some(text),
            })
        }
        Err(e) => {
            if let Some(path) = &cache {
                if let Ok(text) = std::fs::read_to_string(path) {
                    match parse(source, &text) {
                        Ok(config) => {
                            tracing::warn!("{e}; using the cached copy from {}", path.display());
                            return Ok(Loaded {
                                config,
                                text: Some(text),
                            });
                        }
                        Err(cache_err) => tracing::warn!(
                            "cached config at {} is unusable: {cache_err}",
                            path.display()
                        ),
                    }
                }
            }
            tracing::error!(
                "{e}; no usable cached copy, starting with the default config (console only)"
            );
            Ok(Loaded {
                config: Config::default(),
                text: None,
            })
        }
    }
}

/// [`load_initial`] on a throwaway runtime, for callers that are not
/// async and have no client runtime yet.
pub(crate) fn load_blocking(
    source: &ConfigSource,
    opts: &LoadOptions,
) -> Result<Loaded, SignalError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SignalError::Internal(format!("failed to start runtime: {e}")))?
        .block_on(load_initial(source, opts))
}

pub(crate) fn parse(source: &ConfigSource, text: &str) -> Result<Config, SignalError> {
    Config::from_yaml_str(text).map_err(|e| config_err(format!("{source}: {}", strip_prefix(&e))))
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

fn strip_prefix(e: &SignalError) -> String {
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
            "https://x.test/a",
        ] {
            assert_eq!(
                ConfigSource::parse(raw).expect("valid source").to_string(),
                raw
            );
        }
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
    async fn s3_without_the_feature_says_how_to_enable_it() {
        let err = ConfigSource::parse("s3://b/k")
            .expect("valid source")
            .fetch()
            .await
            .expect_err("must be rejected");
        assert!(err.to_string().contains("`s3` cargo feature"));
    }

    #[cfg(not(feature = "gcs"))]
    #[tokio::test]
    async fn gcs_without_the_feature_says_how_to_enable_it() {
        let err = ConfigSource::parse("gs://b/k")
            .expect("valid source")
            .fetch()
            .await
            .expect_err("must be rejected");
        assert!(err.to_string().contains("`gcs` cargo feature"));
    }
}
