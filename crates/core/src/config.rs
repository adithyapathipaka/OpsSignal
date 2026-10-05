//! `signal.yaml` loading.
//!
//! The file is resolved from `$SIGNAL_CONFIG` first, then `./signal.yaml`.
//! If neither exists the SDK still works: it falls back to the
//! zero-config default (console provider, SQLite sink at
//! `./signals.db`), so `notify()` never needs setup to do something
//! useful.
//!
//! `${VAR}` placeholders in string values are replaced from the
//! environment so secrets like Slack webhook URLs stay out of the file.
//! A placeholder whose variable is unset is a config error, not an
//! empty string: a blank webhook URL would only fail later, at delivery
//! time, which is exactly when nobody is looking.

use crate::routing::{Route, RoutingConfig};
use crate::signal::SignalError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const CONFIG_ENV_VAR: &str = "SIGNAL_CONFIG";
pub const DEFAULT_CONFIG_FILE: &str = "signal.yaml";
pub const DEFAULT_SQLITE_PATH: &str = "./signals.db";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub storage: StorageConfig,
}

/// Typed per provider, with unknown keys rejected, so a typo like
/// `slak:` is a load-time error rather than a silently missing provider.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Accepted for readability of config files, but the console
    /// provider is always registered: it is the routing fallback and
    /// cannot be turned off.
    #[serde(default)]
    pub console: Option<ConsoleConfig>,
    #[serde(default)]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackConfig {
    pub webhook_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub url: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SinkKind {
    #[default]
    Sqlite,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default)]
    pub sink: SinkKind,
    #[serde(default = "default_sqlite_path")]
    pub path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            sink: SinkKind::default(),
            path: default_sqlite_path(),
        }
    }
}

fn default_sqlite_path() -> String {
    DEFAULT_SQLITE_PATH.to_string()
}

impl Config {
    /// Parses a config from YAML text, interpolating `${VAR}`
    /// placeholders and checking that every route names a configured
    /// provider.
    pub fn from_yaml_str(text: &str) -> Result<Self, SignalError> {
        let mut value: serde_yaml::Value =
            serde_yaml::from_str(text).map_err(|e| config_err(format!("invalid YAML: {e}")))?;
        // An empty file parses as null; treat it as "all defaults".
        if value.is_null() {
            value = serde_yaml::Value::Mapping(Default::default());
        }
        interpolate_env(&mut value)?;
        let config: Config =
            serde_yaml::from_value(value).map_err(|e| config_err(e.to_string()))?;
        config.check_routes()?;
        Ok(config)
    }

    pub fn from_file(path: &Path) -> Result<Self, SignalError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| config_err(format!("cannot read {}: {e}", path.display())))?;
        Self::from_yaml_str(&text)
            .map_err(|e| config_err(format!("{}: {}", path.display(), strip_prefix(&e))))
    }

    /// `$SIGNAL_CONFIG` if set (and it must then exist), else
    /// `./signal.yaml` if present, else the zero-config default.
    pub fn discover() -> Result<Self, SignalError> {
        match Self::discover_path()? {
            Some(path) => Self::from_file(&path),
            None => Ok(Config::default()),
        }
    }

    fn discover_path() -> Result<Option<PathBuf>, SignalError> {
        if let Some(raw) = std::env::var_os(CONFIG_ENV_VAR) {
            let path = PathBuf::from(raw);
            if !path.is_file() {
                return Err(config_err(format!(
                    "${CONFIG_ENV_VAR} points to {}, which is not a file",
                    path.display()
                )));
            }
            return Ok(Some(path));
        }
        let local = PathBuf::from(DEFAULT_CONFIG_FILE);
        Ok(local.is_file().then_some(local))
    }

    pub fn routing(&self) -> RoutingConfig {
        RoutingConfig {
            routes: self.routes.clone(),
        }
    }

    /// Names of the providers this config makes available. `console`
    /// is always present because routing falls back to it.
    pub fn provider_names(&self) -> Vec<&'static str> {
        let mut names = vec!["console"];
        if self.providers.slack.is_some() {
            names.push("slack");
        }
        if self.providers.webhook.is_some() {
            names.push("webhook");
        }
        names
    }

    fn check_routes(&self) -> Result<(), SignalError> {
        let known = self.provider_names();
        for (i, route) in self.routes.iter().enumerate() {
            if route.providers.is_empty() {
                return Err(config_err(format!("routes[{i}] lists no providers")));
            }
            for name in &route.providers {
                if !known.contains(&name.as_str()) {
                    return Err(config_err(format!(
                        "routes[{i}] references provider '{name}', which is not configured under providers (configured: {})",
                        known.join(", ")
                    )));
                }
            }
        }
        Ok(())
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

fn interpolate_env(value: &mut serde_yaml::Value) -> Result<(), SignalError> {
    match value {
        serde_yaml::Value::String(s) => {
            *s = interpolate_str(s)?;
        }
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                interpolate_env(item)?;
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (_, v) in map.iter_mut() {
                interpolate_env(v)?;
            }
        }
        serde_yaml::Value::Tagged(tagged) => interpolate_env(&mut tagged.value)?,
        _ => {}
    }
    Ok(())
}

fn interpolate_str(input: &str) -> Result<String, SignalError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| config_err(format!("unterminated ${{...}} in '{input}'")))?;
        let name = &after[..end];
        if name.is_empty() {
            return Err(config_err(format!("empty ${{}} placeholder in '{input}'")));
        }
        let val = std::env::var(name).map_err(|_| {
            config_err(format!(
                "environment variable {name} is referenced in signal.yaml but not set"
            ))
        })?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Severity;

    const EXAMPLE: &str = include_str!("../../../python/examples/signal.example.yaml");

    #[test]
    fn example_file_parses_with_env_set() {
        std::env::set_var("SLACK_WEBHOOK_URL", "https://hooks.example.test/abc");
        let config = Config::from_yaml_str(EXAMPLE).expect("example config parses");
        assert_eq!(
            config
                .providers
                .slack
                .as_ref()
                .map(|s| s.webhook_url.as_str()),
            Some("https://hooks.example.test/abc")
        );
        assert_eq!(config.routes.len(), 2);
        assert_eq!(
            config.routes[0].match_rule.severity,
            vec![Severity::Error, Severity::Critical]
        );
        assert_eq!(config.storage.sink, SinkKind::Sqlite);
        assert_eq!(config.storage.path, "./signals.db");
    }

    #[test]
    fn empty_file_is_zero_config_default() {
        let config = Config::from_yaml_str("").expect("empty config parses");
        assert!(config.routes.is_empty());
        assert_eq!(config.provider_names(), vec!["console"]);
        assert_eq!(config.storage.path, DEFAULT_SQLITE_PATH);
    }

    #[test]
    fn interpolates_multiple_placeholders_in_one_value() {
        std::env::set_var("OPSSIGNAL_TEST_HOST", "example.test");
        std::env::set_var("OPSSIGNAL_TEST_PORT", "8443");
        let out = interpolate_str("https://${OPSSIGNAL_TEST_HOST}:${OPSSIGNAL_TEST_PORT}/x")
            .expect("interpolates");
        assert_eq!(out, "https://example.test:8443/x");
    }

    #[test]
    fn missing_env_var_is_a_config_error_naming_the_variable() {
        let yaml = "providers:\n  webhook:\n    url: \"${OPSSIGNAL_TEST_DEFINITELY_UNSET}\"\n";
        let err = Config::from_yaml_str(yaml).expect_err("unset var must fail");
        assert!(matches!(err, SignalError::Config(_)));
        assert!(err.to_string().contains("OPSSIGNAL_TEST_DEFINITELY_UNSET"));
    }

    #[test]
    fn route_to_unconfigured_provider_is_rejected() {
        let yaml = "routes:\n  - match:\n      severity: [error]\n    providers: [slack]\n";
        let err = Config::from_yaml_str(yaml).expect_err("slack is not configured");
        assert!(err.to_string().contains("'slack'"));
    }

    #[test]
    fn unknown_provider_key_is_rejected() {
        let yaml = "providers:\n  slak:\n    webhook_url: x\n";
        assert!(Config::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn unterminated_placeholder_is_rejected() {
        assert!(interpolate_str("${NOPE").is_err());
    }
}
