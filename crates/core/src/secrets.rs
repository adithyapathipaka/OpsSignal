//! Secret lookups in config placeholders.
//!
//! A `${...}` placeholder whose name has a `:` reads a secret store
//! instead of the environment:
//!
//! - `${aws-sm:<secret id or ARN>}`: AWS Secrets Manager
//! - `${gcp-sm:<project>/<secret>[/<version>]}`: GCP Secret Manager
//! - `${azure-kv:<vault>/<name>[/<version>]}`: Azure Key Vault
//!
//! A trailing `#field` reads one field of a JSON secret. Each store is
//! served by its cloud's add-on, like the cloud config sources.
//!
//! Values are cached in memory for `$SIGNAL_SECRETS_TTL_SECS` (300 by
//! default), since every store bills per read and the reloader polls
//! every minute. They are never written to the config cache on disk.

use crate::signal::SignalError;
use crate::source;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const TTL_ENV_VAR: &str = "SIGNAL_SECRETS_TTL_SECS";
const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// The secret stores, as `(scheme, display name)`.
pub const STORES: [(&str, &str); 3] = [
    ("aws-sm", "AWS Secrets Manager"),
    ("gcp-sm", "GCP Secret Manager"),
    ("azure-kv", "Azure Key Vault"),
];

/// Whether a placeholder name (or a URL handed to an add-on) is a
/// secret reference rather than an environment variable.
pub fn is_ref(name: &str) -> bool {
    name.contains(':')
}

/// A parsed `${store:path#field}` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecretRef {
    pub scheme: &'static str,
    /// Store-specific path, such as `prod/slack` or `vault/name`.
    pub path: String,
    pub field: Option<String>,
}

impl SecretRef {
    pub fn parse(name: &str) -> Result<Self, SignalError> {
        let (scheme, rest) = name
            .split_once(':')
            .ok_or_else(|| config_err(format!("'{name}' is not a secret reference")))?;
        let scheme = STORES
            .iter()
            .map(|(s, _)| *s)
            .find(|s| *s == scheme)
            .ok_or_else(|| {
                config_err(format!(
                    "unknown secret store '{scheme}' in ${{{name}}}; use aws-sm, gcp-sm or azure-kv"
                ))
            })?;
        // An ARN has colons but never '#', so the last '#' starts the field.
        let (path, field) = match rest.rsplit_once('#') {
            Some((path, field)) if !field.is_empty() => (path, Some(field.to_string())),
            Some(_) => return Err(config_err(format!("empty #field in ${{{name}}}"))),
            None => (rest, None),
        };
        let parsed = SecretRef {
            scheme,
            path: path.to_string(),
            field,
        };
        parsed.check_path()?;
        Ok(parsed)
    }

    /// The reference without its field: what is fetched and cached, so
    /// several fields of one JSON secret cost one read.
    pub fn url(&self) -> String {
        format!("{}:{}", self.scheme, self.path)
    }

    fn check_path(&self) -> Result<(), SignalError> {
        let bad = |hint: &str| Err(config_err(format!("${{{}}}: expected {hint}", self.url())));
        match self.scheme {
            "aws-sm" if self.path.is_empty() => bad("aws-sm:<secret name or ARN>"),
            "gcp-sm" if gcp_name(&self.path).is_none() => {
                bad("gcp-sm:<project>/<secret>[/<version>] or gcp-sm:projects/<p>/secrets/<s>/versions/<v>")
            }
            "azure-kv" if azure_name(&self.path).is_none() => {
                bad("azure-kv:<vault>/<name>[/<version>]")
            }
            _ => Ok(()),
        }
    }

    fn cloud(&self) -> &'static str {
        match self.scheme {
            "aws-sm" => "aws",
            "gcp-sm" => "gcp",
            _ => "azure",
        }
    }

    /// Whether this build can read the store, natively or through an
    /// add-on. Like an unsupported config scheme, a missing add-on is
    /// an install problem and stops startup.
    fn check_supported(&self) -> Result<(), SignalError> {
        let cloud = self.cloud();
        let built_in = (cloud == "aws" && cfg!(feature = "aws"))
            || (cloud == "gcp" && cfg!(feature = "gcp"))
            || (cloud == "azure" && cfg!(feature = "azure"));
        if built_in || source::has_fetcher(self.scheme) {
            Ok(())
        } else {
            Err(config_err(format!(
                "${{{}}}: {}",
                self.url(),
                source::missing_support(self.scheme)
            )))
        }
    }
}

/// `(project, secret, version)` from either path form.
pub(crate) fn gcp_name(path: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = path.split('/').collect();
    let named = match parts.as_slice() {
        ["projects", p, "secrets", s, "versions", v] => (*p, *s, *v),
        ["projects", p, "secrets", s] => (*p, *s, "latest"),
        [p, s, v] => (*p, *s, *v),
        [p, s] => (*p, *s, "latest"),
        _ => return None,
    };
    let (p, s, v) = named;
    (!p.is_empty() && !s.is_empty() && !v.is_empty()).then_some(named)
}

/// `(vault, name, version)`; an empty version is the current one.
pub(crate) fn azure_name(path: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = path.split('/').collect();
    let named = match parts.as_slice() {
        [vault, name] => (*vault, *name, ""),
        [vault, name, version] if !version.is_empty() => (*vault, *name, *version),
        _ => return None,
    };
    (!named.0.is_empty() && !named.1.is_empty()).then_some(named)
}

/// Resolved secrets, by placeholder name.
pub(crate) type Values = HashMap<String, String>;

pub(crate) struct ResolveError {
    pub err: SignalError,
    /// A malformed reference or a missing add-on; a store that can't be
    /// reached is not fatal.
    pub fatal: bool,
}

/// Resolves every reference in `names`. Each distinct secret is read at
/// most once per TTL.
pub(crate) async fn resolve(names: &[String]) -> Result<Values, ResolveError> {
    let fatal = |err| ResolveError { err, fatal: true };
    let mut refs = Vec::with_capacity(names.len());
    for name in names {
        let parsed = SecretRef::parse(name).map_err(fatal)?;
        parsed.check_supported().map_err(fatal)?;
        refs.push((name, parsed));
    }
    let mut raw: HashMap<String, String> = HashMap::new();
    let mut values = Values::new();
    for (name, secret) in refs {
        let url = secret.url();
        if !raw.contains_key(&url) {
            let value = cached_read(&url).await.map_err(|e| ResolveError {
                err: config_err(format!("${{{url}}}: {}", source::message(&e))),
                fatal: false,
            })?;
            raw.insert(url.clone(), value);
        }
        let value = &raw[&url];
        let value = match &secret.field {
            Some(field) => json_field(value, field).map_err(|e| ResolveError {
                err: config_err(format!("${{{name}}}: {e}")),
                fatal: false,
            })?,
            None => value.clone(),
        };
        values.insert(name.clone(), value);
    }
    Ok(values)
}

/// A fingerprint of the resolved values, so the reloader notices a
/// rotated secret without keeping the values around.
pub(crate) fn digest(values: &Values) -> String {
    if values.is_empty() {
        return String::new();
    }
    let sorted: BTreeMap<_, _> = values.iter().collect();
    let mut hasher = Sha256::new();
    for (name, value) in sorted {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn json_field(secret: &str, field: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(secret)
        .map_err(|_| format!("the secret is not JSON, so it has no field '{field}'"))?;
    match value.get(field) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(serde_json::Value::Null) | None => Err(format!("the secret has no field '{field}'")),
        Some(other) => Ok(other.to_string()),
    }
}

type Cache = Mutex<HashMap<String, (Instant, String)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn ttl() -> Duration {
    std::env::var(TTL_ENV_VAR)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .map_or(DEFAULT_TTL, Duration::from_secs)
}

async fn cached_read(url: &str) -> Result<String, SignalError> {
    let ttl = ttl();
    if let Some((at, value)) = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(url)
        .cloned()
    {
        if at.elapsed() < ttl {
            return Ok(value);
        }
    }
    let value = read(url).await?;
    if !ttl.is_zero() {
        cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(url.to_string(), (Instant::now(), value.clone()));
    }
    Ok(value)
}

/// Reads one secret (`scheme:path`, no field) without the cache. Add-on
/// packages reach this through [`source::fetch_blocking`].
pub(crate) async fn read(url: &str) -> Result<String, SignalError> {
    let secret = SecretRef::parse(url)?;
    match secret.scheme {
        #[cfg(feature = "aws")]
        "aws-sm" => crate::cloud::aws::fetch_secret(&secret.path).await,
        #[cfg(feature = "gcp")]
        "gcp-sm" => {
            let (project, name, version) = gcp_name(&secret.path).expect("checked by parse");
            crate::cloud::gcp::fetch_secret(project, name, version).await
        }
        #[cfg(feature = "azure")]
        "azure-kv" => {
            let (vault, name, version) = azure_name(&secret.path).expect("checked by parse");
            crate::cloud::azure::fetch_secret(vault, name, version).await
        }
        scheme => source::fetch_external(scheme, secret.url()).await,
    }
}

fn config_err(msg: impl Into<String>) -> SignalError {
    SignalError::Config(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_references() {
        let arn =
            SecretRef::parse("aws-sm:arn:aws:secretsmanager:eu-west-1:1:secret:prod/slack-AbC#url")
                .expect("parses");
        assert_eq!(arn.scheme, "aws-sm");
        assert_eq!(
            arn.path,
            "arn:aws:secretsmanager:eu-west-1:1:secret:prod/slack-AbC"
        );
        assert_eq!(arn.field.as_deref(), Some("url"));
        assert_eq!(
            arn.url(),
            "aws-sm:arn:aws:secretsmanager:eu-west-1:1:secret:prod/slack-AbC"
        );

        let gcp = SecretRef::parse("gcp-sm:my-proj/slack").expect("parses");
        assert_eq!(gcp_name(&gcp.path), Some(("my-proj", "slack", "latest")));
        assert_eq!(
            gcp_name("projects/p/secrets/s/versions/3"),
            Some(("p", "s", "3"))
        );
        assert_eq!(gcp_name("p/s/7"), Some(("p", "s", "7")));

        let kv = SecretRef::parse("azure-kv:ops-vault/slack-url").expect("parses");
        assert_eq!(azure_name(&kv.path), Some(("ops-vault", "slack-url", "")));
        assert_eq!(azure_name("v/n/abc123"), Some(("v", "n", "abc123")));
    }

    #[test]
    fn rejects_bad_references() {
        for bad in [
            "vault:x",
            "aws-sm:",
            "aws-sm:x#",
            "gcp-sm:only-project",
            "gcp-sm:a/b/c/d",
            "gcp-sm:/s",
            "azure-kv:vault",
            "azure-kv:vault/name/",
            "azure-kv:/name",
        ] {
            assert!(SecretRef::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn reads_json_fields() {
        let secret = r#"{"url":"https://hooks.example.test/x","port":8443}"#;
        assert_eq!(
            json_field(secret, "url").unwrap(),
            "https://hooks.example.test/x"
        );
        assert_eq!(json_field(secret, "port").unwrap(), "8443");
        assert!(json_field(secret, "missing").is_err());
        assert!(json_field("plain", "url").is_err());
    }

    #[test]
    fn digest_ignores_order_and_changes_with_values() {
        let a: Values = [("x".into(), "1".into()), ("y".into(), "2".into())].into();
        let b: Values = [("y".into(), "2".into()), ("x".into(), "1".into())].into();
        let c: Values = [("x".into(), "1".into()), ("y".into(), "3".into())].into();
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&c));
        assert_eq!(digest(&Values::new()), "");
    }
}
