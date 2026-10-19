//! `signal.yaml` loading.
//!
//! The file is resolved from `$SIGNAL_CONFIG` first (a local path, or an
//! `https://`, `s3://`, `gs://` or `az://` URL; see [`crate::source`]), then
//! `./signal.yaml`.
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
use crate::source::{self, ConfigSource, LoadOptions};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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

/// One file of a layered config: what it sets itself, plus the files
/// it builds on. [`Config::from_layers`] merges a chain of these.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// Parent files, as URLs or paths relative to this file. Each
    /// parent is merged in before this file, so this file wins.
    #[serde(default, deserialize_with = "one_or_many")]
    pub extends: Vec<String>,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

impl Layer {
    /// Parses one file, interpolating `${VAR}` placeholders. Routes are
    /// checked only once the whole chain is merged, since a team file
    /// may route to a provider its parent configures.
    ///
    /// A `${store:...}` secret placeholder is an error here, since it
    /// needs a network read; configs loaded through a client resolve
    /// them first (see [`crate::secrets`]).
    pub fn from_yaml_str(text: &str) -> Result<Self, SignalError> {
        Self::from_yaml_str_with(text, &HashMap::new())
    }

    /// Like [`Layer::from_yaml_str`], taking secret placeholders from
    /// `secrets` (keyed by placeholder name, such as `aws-sm:prod/x`).
    pub(crate) fn from_yaml_str_with(
        text: &str,
        secrets: &HashMap<String, String>,
    ) -> Result<Self, SignalError> {
        let mut value = parse_value(text)?;
        interpolate(&mut value, secrets)?;
        serde_yaml::from_value(value).map_err(|e| config_err(e.to_string()))
    }

    /// The names of the secret placeholders in `text`, each once, in
    /// order. Placeholders in YAML comments are not included.
    pub(crate) fn secret_refs(text: &str) -> Result<Vec<String>, SignalError> {
        fn walk(value: &serde_yaml::Value, out: &mut Vec<String>) -> Result<(), SignalError> {
            match value {
                serde_yaml::Value::String(s) => {
                    for name in placeholders(s)? {
                        if crate::secrets::is_ref(name) && !out.iter().any(|n| n == name) {
                            out.push(name.to_string());
                        }
                    }
                }
                serde_yaml::Value::Sequence(items) => {
                    for item in items {
                        walk(item, out)?;
                    }
                }
                serde_yaml::Value::Mapping(map) => {
                    for (_, v) in map {
                        walk(v, out)?;
                    }
                }
                serde_yaml::Value::Tagged(tagged) => walk(&tagged.value, out)?,
                _ => {}
            }
            Ok(())
        }
        let mut out = Vec::new();
        walk(&parse_value(text)?, &mut out)?;
        Ok(out)
    }
}

fn parse_value(text: &str) -> Result<serde_yaml::Value, SignalError> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| config_err(format!("invalid YAML: {e}")))?;
    // An empty file parses as null; treat it as "all defaults".
    Ok(if value.is_null() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        value
    })
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
    #[serde(default)]
    pub pagerduty: Option<PagerDutyConfig>,
    #[serde(default)]
    pub teams: Option<TeamsConfig>,
    #[serde(default)]
    pub opsgenie: Option<OpsgenieConfig>,
    #[serde(default)]
    pub email: Option<EmailConfig>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagerDutyConfig {
    /// Integration key of an Events API v2 integration on a service.
    pub routing_key: String,
    /// Events API endpoint; defaults to the US one. EU accounts set
    /// `https://events.eu.pagerduty.com/v2/enqueue`.
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamsConfig {
    /// Workflows (or legacy incoming) webhook URL of a Teams channel.
    pub webhook_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsgenieConfig {
    /// Key of an API integration on an Opsgenie team.
    pub api_key: String,
    /// Alert API endpoint; defaults to the US one. EU accounts set
    /// `https://api.eu.opsgenie.com/v2/alerts`.
    #[serde(default)]
    pub url: Option<String>,
    /// Added to every alert, before the signal's severity and
    /// environment.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Serialize as well, so a build without the AWS feature can hand the
/// config to the `opssignal[aws]` add-on.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmailConfig {
    /// Sender, as `alerts@example.com` or `OpsSignal <alerts@example.com>`.
    pub from: String,
    /// At least one recipient.
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    /// Put before the subject, such as `[opssignal]`, for mail filters.
    #[serde(default)]
    pub subject_prefix: Option<String>,
    /// Send through an SMTP server. Exactly one of `smtp` and `ses`.
    #[serde(default)]
    pub smtp: Option<SmtpConfig>,
    /// Send through the Amazon SES API with the AWS credential chain;
    /// needs the `aws` add-on.
    #[serde(default)]
    pub ses: Option<SesConfig>,
}

/// How an [`EmailConfig`] sends.
pub enum EmailTransport<'a> {
    Smtp(&'a SmtpConfig),
    Ses(&'a SesConfig),
}

impl EmailConfig {
    /// The transport; [`EmailConfig::check`] makes sure there is
    /// exactly one.
    pub fn transport(&self) -> Option<EmailTransport<'_>> {
        match (&self.smtp, &self.ses) {
            (Some(smtp), None) => Some(EmailTransport::Smtp(smtp)),
            (None, Some(ses)) => Some(EmailTransport::Ses(ses)),
            _ => None,
        }
    }

    /// Addresses are checked at load time, since a bad one would fail
    /// every send.
    fn check(&self) -> Result<(), SignalError> {
        if self.to.is_empty() {
            return Err(config_err("providers.email.to lists no recipients"));
        }
        let addresses = std::iter::once(&self.from).chain(&self.to).chain(&self.cc);
        for address in addresses {
            address.parse::<lettre::message::Mailbox>().map_err(|e| {
                config_err(format!("providers.email: invalid address {address:?}: {e}"))
            })?;
        }
        match self.transport() {
            Some(EmailTransport::Smtp(smtp)) => {
                if smtp.username.is_some() != smtp.password.is_some() {
                    return Err(config_err(
                        "providers.email.smtp needs both username and password, or neither",
                    ));
                }
            }
            Some(EmailTransport::Ses(_)) => {
                if !crate::providers::external::cloud_supported("aws") {
                    return Err(config_err(format!(
                        "providers.email: {}",
                        crate::providers::external::missing_support("email over Amazon SES", "aws")
                    )));
                }
            }
            None => {
                return Err(config_err(
                    "providers.email needs exactly one of smtp and ses",
                ))
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SesConfig {
    /// Defaults to `AWS_REGION`, then `AWS_DEFAULT_REGION`, then
    /// `us-east-1`.
    #[serde(default)]
    pub region: Option<String>,
    /// SES configuration set, for event publishing and dedicated IPs.
    #[serde(default)]
    pub configuration_set: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    pub host: String,
    /// Defaults to 587 for `starttls`, 465 for `tls` and 25 for `none`.
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub tls: SmtpTls,
}

impl SmtpConfig {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(match self.tls {
            SmtpTls::Starttls => 587,
            SmtpTls::Tls => 465,
            SmtpTls::None => 25,
        })
    }
}

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS, which must succeed.
    #[default]
    Starttls,
    /// TLS from the first byte (SMTPS).
    Tls,
    /// No encryption, for a relay on localhost or inside the network.
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SinkKind {
    #[default]
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    /// Parses a single, self-contained config from YAML text,
    /// interpolating `${VAR}` placeholders and checking that every route
    /// names a configured provider. A file that uses `extends` needs to
    /// be loaded from where it lives, with [`Config::from_file`] or
    /// `$SIGNAL_CONFIG`, so its parents can be found.
    pub fn from_yaml_str(text: &str) -> Result<Self, SignalError> {
        let layer = Layer::from_yaml_str(text)?;
        if !layer.extends.is_empty() {
            return Err(config_err(
                "`extends` needs the file's location; load it with Config::from_file or $SIGNAL_CONFIG",
            ));
        }
        Self::from_layers([(None, layer)])
    }

    /// Merges a chain of layers, given lowest priority first, each with
    /// the name of the file it came from (for error messages):
    ///
    /// - providers merge by name, and a later layer's entry replaces an
    ///   earlier one's;
    /// - a later layer's routes go before the earlier layers' routes,
    ///   so the more specific file is matched first and the shared
    ///   defaults still catch everything else;
    /// - `storage` comes from the last layer that sets it.
    ///
    /// Routes are checked against the merged providers.
    pub fn from_layers(
        layers: impl IntoIterator<Item = (Option<String>, Layer)>,
    ) -> Result<Self, SignalError> {
        let mut config = Config::default();
        // (file, index within that file) for each merged route.
        let mut origins: Vec<(Option<String>, usize)> = Vec::new();
        for (name, layer) in layers {
            let p = layer.providers;
            config.providers.console = p.console.or(config.providers.console.take());
            config.providers.slack = p.slack.or(config.providers.slack.take());
            config.providers.webhook = p.webhook.or(config.providers.webhook.take());
            config.providers.pagerduty = p.pagerduty.or(config.providers.pagerduty.take());
            config.providers.teams = p.teams.or(config.providers.teams.take());
            config.providers.opsgenie = p.opsgenie.or(config.providers.opsgenie.take());
            config.providers.email = p.email.or(config.providers.email.take());
            if let Some(storage) = layer.storage {
                config.storage = storage;
            }
            let mut routes = layer.routes;
            let mut route_origins: Vec<_> = (0..routes.len()).map(|i| (name.clone(), i)).collect();
            routes.append(&mut config.routes);
            route_origins.append(&mut origins);
            config.routes = routes;
            origins = route_origins;
        }
        if let Some(email) = &config.providers.email {
            email.check()?;
        }
        config.check_routes(&origins)?;
        Ok(config)
    }

    pub fn from_file(path: &Path) -> Result<Self, SignalError> {
        let source = ConfigSource::File(path.to_path_buf());
        source::load_blocking(std::slice::from_ref(&source), &LoadOptions::once()).map(|l| l.config)
    }

    /// `$SIGNAL_CONFIG` if set (one or more comma-separated local
    /// files, which must then exist, or remote URLs), else
    /// `./signal.yaml` if present, else the zero-config default. Remote
    /// files are fetched once, with the cache fallback described in
    /// [`crate::source`].
    pub fn discover() -> Result<Self, SignalError> {
        match ConfigSource::discover()? {
            Some(sources) => {
                source::load_blocking(&sources, &LoadOptions::from_env()?).map(|l| l.config)
            }
            None => Ok(Config::default()),
        }
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
        if self.providers.pagerduty.is_some() {
            names.push("pagerduty");
        }
        if self.providers.teams.is_some() {
            names.push("teams");
        }
        if self.providers.opsgenie.is_some() {
            names.push("opsgenie");
        }
        if self.providers.email.is_some() {
            names.push("email");
        }
        names
    }

    fn check_routes(&self, origins: &[(Option<String>, usize)]) -> Result<(), SignalError> {
        let known = self.provider_names();
        for (route, (file, i)) in self.routes.iter().zip(origins) {
            let at = match file {
                Some(file) => format!("{file}: routes[{i}]"),
                None => format!("routes[{i}]"),
            };
            if route.providers.is_empty() {
                return Err(config_err(format!("{at} lists no providers")));
            }
            for name in &route.providers {
                if !known.contains(&name.as_str()) {
                    return Err(config_err(format!(
                        "{at} references provider '{name}', which is not configured under providers (configured: {})",
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

fn interpolate(
    value: &mut serde_yaml::Value,
    secrets: &HashMap<String, String>,
) -> Result<(), SignalError> {
    match value {
        serde_yaml::Value::String(s) => {
            *s = interpolate_str(s, secrets)?;
        }
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                interpolate(item, secrets)?;
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (_, v) in map.iter_mut() {
                interpolate(v, secrets)?;
            }
        }
        serde_yaml::Value::Tagged(tagged) => interpolate(&mut tagged.value, secrets)?,
        _ => {}
    }
    Ok(())
}

/// The `${...}` names in `input`, in order.
fn placeholders(input: &str) -> Result<Vec<&str>, SignalError> {
    let mut names = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| config_err(format!("unterminated ${{...}} in '{input}'")))?;
        let name = &after[..end];
        if name.is_empty() {
            return Err(config_err(format!("empty ${{}} placeholder in '{input}'")));
        }
        names.push(name);
        rest = &after[end + 1..];
    }
    Ok(names)
}

fn interpolate_str(input: &str, secrets: &HashMap<String, String>) -> Result<String, SignalError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    for name in placeholders(input)? {
        let placeholder = format!("${{{name}}}");
        let start = rest.find(&placeholder).expect("found by placeholders");
        out.push_str(&rest[..start]);
        let val = if crate::secrets::is_ref(name) {
            secrets.get(name).cloned().ok_or_else(|| {
                config_err(format!(
                    "secret ${{{name}}} can only be read when the config is loaded by a client"
                ))
            })?
        } else {
            std::env::var(name).map_err(|_| {
                config_err(format!(
                    "environment variable {name} is referenced in signal.yaml but not set"
                ))
            })?
        };
        out.push_str(&val);
        rest = &rest[start + placeholder.len()..];
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
        let out = interpolate_str(
            "https://${OPSSIGNAL_TEST_HOST}:${OPSSIGNAL_TEST_PORT}/x",
            &HashMap::new(),
        )
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
        assert!(interpolate_str("${NOPE", &HashMap::new()).is_err());
    }

    #[test]
    fn route_with_no_providers_is_rejected() {
        let yaml = "routes:\n  - match:\n      severity: [error]\n    providers: []\n";
        let err = Config::from_yaml_str(yaml).expect_err("empty provider list");
        assert!(err.to_string().contains("routes[0] lists no providers"));
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        assert!(Config::from_yaml_str("providerz: {}\n").is_err());
    }

    #[test]
    fn unknown_sink_kind_is_rejected() {
        assert!(Config::from_yaml_str("storage:\n  sink: postgres\n").is_err());
    }

    #[test]
    fn invalid_yaml_is_a_config_error() {
        let err = Config::from_yaml_str("routes: [unclosed\n").expect_err("bad YAML");
        assert!(matches!(err, SignalError::Config(_)));
        assert!(err.to_string().contains("invalid YAML"));
    }

    #[test]
    fn secret_placeholders_are_collected_from_values_only() {
        let yaml = "# old: ${aws-sm:retired}\nproviders:\n  slack:\n    webhook_url: \"${aws-sm:prod/slack#url}\"\n  webhook:\n    url: \"https://${OPSSIGNAL_TEST_HOST}/${gcp-sm:ops/path}?k=${aws-sm:prod/slack#url}\"\n";
        assert_eq!(
            Layer::secret_refs(yaml).expect("refs"),
            ["aws-sm:prod/slack#url", "gcp-sm:ops/path"]
        );
    }

    #[test]
    fn secret_placeholders_take_resolved_values() {
        std::env::set_var("OPSSIGNAL_TEST_SECRET_HOST", "hooks.example.test");
        let yaml = "providers:\n  webhook:\n    url: \"https://${OPSSIGNAL_TEST_SECRET_HOST}/${azure-kv:v/path}\"\n";
        let err = Layer::from_yaml_str(yaml).expect_err("unresolved secret");
        assert!(err.to_string().contains("${azure-kv:v/path}"), "{err}");

        let secrets = HashMap::from([("azure-kv:v/path".to_string(), "abc".to_string())]);
        let layer = Layer::from_yaml_str_with(yaml, &secrets).expect("layer");
        assert_eq!(
            layer.providers.webhook.expect("webhook").url,
            "https://hooks.example.test/abc"
        );
    }

    #[test]
    fn empty_placeholder_is_rejected() {
        assert!(interpolate_str("x${}y", &HashMap::new()).is_err());
    }

    #[test]
    fn text_without_placeholders_is_unchanged() {
        assert_eq!(
            interpolate_str("https://example.test/$5{x", &HashMap::new()).expect("no placeholder"),
            "https://example.test/$5{x"
        );
    }

    #[test]
    fn placeholders_inside_lists_are_interpolated() {
        std::env::set_var("OPSSIGNAL_TEST_ENV_NAME", "prod");
        let yaml = "providers:\n  webhook:\n    url: https://example.test\nroutes:\n  - match:\n      environment: \"${OPSSIGNAL_TEST_ENV_NAME}\"\n    providers: [webhook]\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        assert_eq!(
            config.routes[0].match_rule.environment.as_deref(),
            Some("prod")
        );
    }

    #[test]
    fn provider_names_follow_configured_providers() {
        let yaml = "providers:\n  webhook:\n    url: https://example.test\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        assert_eq!(config.provider_names(), vec!["console", "webhook"]);
    }

    #[test]
    fn pagerduty_and_teams_parse_and_can_be_routed_to() {
        let yaml = "providers:\n  pagerduty:\n    routing_key: R0UT1NG\n  teams:\n    webhook_url: https://example.test/teams\nroutes:\n  - match:\n      severity: [critical]\n    providers: [pagerduty, teams]\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        let pd = config.providers.pagerduty.as_ref().expect("pagerduty");
        assert_eq!(pd.routing_key, "R0UT1NG");
        assert_eq!(pd.url, None);
        assert_eq!(
            config.providers.teams.as_ref().expect("teams").webhook_url,
            "https://example.test/teams"
        );
        assert_eq!(
            config.provider_names(),
            vec!["console", "pagerduty", "teams"]
        );
    }

    #[test]
    fn pagerduty_accepts_an_eu_endpoint() {
        let yaml = "providers:\n  pagerduty:\n    routing_key: k\n    url: https://events.eu.pagerduty.com/v2/enqueue\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        assert_eq!(
            config
                .providers
                .pagerduty
                .expect("pagerduty")
                .url
                .as_deref(),
            Some("https://events.eu.pagerduty.com/v2/enqueue")
        );
    }

    #[test]
    fn pagerduty_without_a_routing_key_is_rejected() {
        let yaml = "providers:\n  pagerduty:\n    url: https://example.test\n";
        let err = Config::from_yaml_str(yaml).expect_err("routing_key is required");
        assert!(err.to_string().contains("routing_key"), "{err}");
    }

    #[test]
    fn unknown_teams_field_is_rejected() {
        let yaml = "providers:\n  teams:\n    url: https://example.test\n";
        let err = Config::from_yaml_str(yaml).expect_err("teams takes webhook_url");
        assert!(err.to_string().contains("url"), "{err}");
    }

    #[test]
    fn later_layer_replaces_pagerduty_and_teams() {
        let base = Layer::from_yaml_str(
            "providers:\n  pagerduty:\n    routing_key: org\n  teams:\n    webhook_url: https://example.test/org\n",
        )
        .expect("base");
        let team = Layer::from_yaml_str("providers:\n  pagerduty:\n    routing_key: payments\n")
            .expect("team");
        let config = Config::from_layers([(None, base), (None, team)]).expect("merged");
        assert_eq!(
            config.providers.pagerduty.expect("pagerduty").routing_key,
            "payments"
        );
        assert_eq!(
            config.providers.teams.expect("teams").webhook_url,
            "https://example.test/org"
        );
    }

    #[test]
    fn opsgenie_parses_with_tags_and_an_eu_endpoint() {
        let yaml = "providers:\n  opsgenie:\n    api_key: K3Y\n    url: https://api.eu.opsgenie.com/v2/alerts\n    tags: [data, etl]\nroutes:\n  - match:\n      severity: [critical]\n    providers: [opsgenie]\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        let og = config.providers.opsgenie.as_ref().expect("opsgenie");
        assert_eq!(og.api_key, "K3Y");
        assert_eq!(
            og.url.as_deref(),
            Some("https://api.eu.opsgenie.com/v2/alerts")
        );
        assert_eq!(og.tags, vec!["data", "etl"]);
        assert_eq!(config.provider_names(), vec!["console", "opsgenie"]);
    }

    #[test]
    fn opsgenie_without_an_api_key_is_rejected() {
        let yaml = "providers:\n  opsgenie:\n    tags: [data]\n";
        let err = Config::from_yaml_str(yaml).expect_err("api_key is required");
        assert!(err.to_string().contains("api_key"), "{err}");
    }

    #[test]
    fn email_parses_with_smtp_defaults() {
        let yaml = "providers:\n  email:\n    from: \"OpsSignal <alerts@example.test>\"\n    to: [oncall@example.test]\n    smtp:\n      host: smtp.example.test\n      username: u\n      password: p\nroutes:\n  - match:\n      severity: [error]\n    providers: [email]\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        let email = config.providers.email.as_ref().expect("email");
        assert_eq!(email.to, vec!["oncall@example.test"]);
        assert!(email.cc.is_empty());
        let smtp = email.smtp.as_ref().expect("smtp");
        assert_eq!(smtp.tls, SmtpTls::Starttls);
        assert_eq!(smtp.port(), 587);
        assert_eq!(config.provider_names(), vec!["console", "email"]);
    }

    #[test]
    fn email_tls_modes_parse() {
        let yaml = "providers:\n  email:\n    from: a@example.test\n    to: [b@example.test]\n    smtp:\n      host: relay.internal\n      tls: none\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        let smtp = config.providers.email.expect("email").smtp.expect("smtp");
        assert_eq!(smtp.tls, SmtpTls::None);
        assert_eq!(smtp.port(), 25);
    }

    #[test]
    fn email_with_a_bad_address_is_rejected_at_load() {
        let yaml = "providers:\n  email:\n    from: a@example.test\n    to: [not-an-address]\n    smtp:\n      host: h\n";
        let err = Config::from_yaml_str(yaml).expect_err("bad address");
        assert!(err.to_string().contains("not-an-address"), "{err}");
    }

    #[test]
    fn email_without_recipients_is_rejected() {
        let yaml = "providers:\n  email:\n    from: a@example.test\n    to: []\n    smtp:\n      host: h\n";
        let err = Config::from_yaml_str(yaml).expect_err("no recipients");
        assert!(err.to_string().contains("recipients"), "{err}");
    }

    #[test]
    fn email_username_without_password_is_rejected() {
        let yaml = "providers:\n  email:\n    from: a@example.test\n    to: [b@example.test]\n    smtp:\n      host: h\n      username: u\n";
        let err = Config::from_yaml_str(yaml).expect_err("half credentials");
        assert!(err.to_string().contains("password"), "{err}");
    }

    #[test]
    fn email_needs_exactly_one_transport() {
        let neither = "providers:\n  email:\n    from: a@example.test\n    to: [b@example.test]\n";
        let err = Config::from_yaml_str(neither).expect_err("no transport");
        assert!(err.to_string().contains("exactly one"), "{err}");
        let both = "providers:\n  email:\n    from: a@example.test\n    to: [b@example.test]\n    smtp:\n      host: h\n    ses: {}\n";
        let err = Config::from_yaml_str(both).expect_err("two transports");
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[cfg(feature = "aws")]
    #[test]
    fn email_over_ses_parses_in_an_aws_build() {
        let yaml = "providers:\n  email:\n    from: a@example.test\n    to: [b@example.test]\n    ses:\n      region: eu-west-1\n      configuration_set: alerts\n";
        let config = Config::from_yaml_str(yaml).expect("config");
        let ses = config.providers.email.expect("email").ses.expect("ses");
        assert_eq!(ses.region.as_deref(), Some("eu-west-1"));
        assert_eq!(ses.configuration_set.as_deref(), Some("alerts"));
    }

    #[test]
    fn from_file_reports_the_path_on_error() {
        let missing = std::env::temp_dir().join("opssignal-definitely-missing.yaml");
        let err = Config::from_file(&missing).expect_err("missing file");
        assert!(err
            .to_string()
            .contains("opssignal-definitely-missing.yaml"));

        let bad = std::env::temp_dir().join(format!("opssignal-bad-{}.yaml", std::process::id()));
        std::fs::write(&bad, "providers:\n  slak: {}\n").expect("write");
        let err = Config::from_file(&bad).expect_err("bad file");
        assert!(err.to_string().contains(&*bad.to_string_lossy()));
        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn discover_honours_signal_config_and_rejects_a_missing_file() {
        // The only test that touches $SIGNAL_CONFIG, so it cannot race
        // another test in this binary.
        let path =
            std::env::temp_dir().join(format!("opssignal-discover-{}.yaml", std::process::id()));
        std::fs::write(&path, "storage:\n  path: /tmp/discovered.db\n").expect("write");

        std::env::set_var(CONFIG_ENV_VAR, &path);
        let found = Config::discover();
        std::env::set_var(CONFIG_ENV_VAR, path.with_extension("missing"));
        let missing = Config::discover();
        std::env::remove_var(CONFIG_ENV_VAR);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            found.expect("discovered").storage.path,
            "/tmp/discovered.db"
        );
        let err = missing.expect_err("missing $SIGNAL_CONFIG target");
        assert!(err.to_string().contains("not a file"));
    }
}
