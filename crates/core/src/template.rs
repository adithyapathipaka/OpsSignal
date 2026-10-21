//! Per-provider message templates.
//!
//! Each provider entry in `signal.yaml` may carry a `template` map that
//! replaces the text of one or more parts of the message it sends, such
//! as Slack's `text` or PagerDuty's `summary`. Templates use Jinja
//! syntax (rendered with minijinja), and see every signal field by name:
//! `{{ title }}`, `{{ severity }}`, `{{ metadata.dag_id }}`.
//!
//! Anything a template does not set keeps the built-in format, so a
//! config without templates sends exactly what it always did.
//!
//! Mistakes are caught when the config loads: a syntax error, a part
//! the provider doesn't have (`providers.slack.template.txt`) or an
//! unknown variable (`{{ tittle }}`) is a config error. A template that
//! still fails at send time, or renders to nothing but whitespace, is
//! logged and the built-in format is sent instead: a broken template
//! must never cost the alert.

use crate::signal::{Signal, SignalError};
use minijinja::{Environment, Value};
use std::collections::BTreeMap;

/// A provider's `template` map: part name to template source.
pub type TemplateConfig = BTreeMap<String, String>;

/// Signal fields a template can use. Optional fields the signal doesn't
/// set are undefined, so `{% if message %}` works and `{{ message }}`
/// renders as nothing.
pub const SIGNAL_VARIABLES: &[&str] = &[
    "id",
    "source",
    "service",
    "component",
    "event_type",
    "severity",
    "title",
    "message",
    "environment",
    "team",
    "run_id",
    "correlation_id",
    "trace_id",
    "log_url",
    "metadata",
    "timestamp",
    "dedup_key",
    "action",
];

/// The parts each provider lets a template replace, and any variables
/// it adds on top of the signal fields.
const PROVIDERS: &[(&str, &[&str], &[&str])] = &[
    ("console", &["text"], &[]),
    ("slack", &["text"], &["emoji"]),
    ("webhook", &["body"], &[]),
    ("pagerduty", &["summary"], &[]),
    ("teams", &["title", "text"], &[]),
    ("opsgenie", &["message", "description"], &[]),
    ("email", &["subject", "body"], &[]),
];

fn provider_spec(provider: &str) -> (&'static [&'static str], &'static [&'static str]) {
    PROVIDERS
        .iter()
        .find(|(name, _, _)| *name == provider)
        .map(|(_, parts, extras)| (*parts, *extras))
        .unwrap_or((&[], &[]))
}

/// One provider's compiled templates.
pub struct Templates {
    provider: &'static str,
    env: Environment<'static>,
}

impl std::fmt::Debug for Templates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Templates")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl Templates {
    /// Compiles and checks `provider`'s templates; `None` when it has
    /// none. Used at config load, so every error names the offending
    /// key.
    pub fn compile(
        provider: &'static str,
        config: &TemplateConfig,
    ) -> Result<Option<Self>, SignalError> {
        if config.is_empty() {
            return Ok(None);
        }
        let (parts, extras) = provider_spec(provider);
        // The default trims one trailing newline, the one a YAML `|`
        // block adds.
        let mut env = Environment::new();
        for (part, source) in config {
            let at = format!("providers.{provider}.template.{part}");
            if !parts.contains(&part.as_str()) {
                return Err(config_err(format!(
                    "{at}: {provider} has no part '{part}' (templatable: {})",
                    parts.join(", ")
                )));
            }
            env.add_template_owned(part.clone(), source.clone())
                .map_err(|e| config_err(format!("{at}: {e}")))?;
            let template = env
                .get_template(part)
                .map_err(|e| config_err(format!("{at}: {e}")))?;
            let mut unknown: Vec<String> = template
                .undeclared_variables(false)
                .into_iter()
                .filter(|name| {
                    !SIGNAL_VARIABLES.contains(&name.as_str())
                        && !extras.contains(&name.as_str())
                        && !env.globals().any(|(global, _)| global == name)
                })
                .collect();
            if !unknown.is_empty() {
                unknown.sort();
                let mut known: Vec<&str> = SIGNAL_VARIABLES.to_vec();
                known.extend_from_slice(extras);
                return Err(config_err(format!(
                    "{at}: unknown variable {} (available: {})",
                    unknown.join(", "),
                    known.join(", ")
                )));
            }
        }
        Ok(Some(Templates { provider, env }))
    }

    /// Like [`Templates::compile`], for building a provider from a
    /// config that was checked when it loaded. An error here can only
    /// come from a config built by hand; it is logged and the provider
    /// keeps its built-in format.
    pub fn load(provider: &'static str, config: &TemplateConfig) -> Option<Self> {
        match Self::compile(provider, config) {
            Ok(templates) => templates,
            Err(e) => {
                tracing::warn!(provider, error = %e, "ignoring message templates");
                None
            }
        }
    }

    /// Renders `part` for `signal`, or `None` when this provider has no
    /// template for it, it fails, or it renders to only whitespace; the
    /// caller then uses its built-in format.
    pub fn render(&self, part: &str, signal: &Signal, extras: &[(&str, Value)]) -> Option<String> {
        let template = self.env.get_template(part).ok()?;
        let result = template.render(context(signal, extras));
        match result {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => {
                tracing::warn!(
                    provider = self.provider,
                    part,
                    signal_id = %signal.id,
                    "template rendered empty; sending the default format"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    provider = self.provider,
                    part,
                    signal_id = %signal.id,
                    error = %e,
                    "template failed; sending the default format"
                );
                None
            }
        }
    }
}

/// Renders `part` with `templates` if there are any, else `None`.
pub fn render(
    templates: &Option<Templates>,
    part: &str,
    signal: &Signal,
    extras: &[(&str, Value)],
) -> Option<String> {
    templates.as_ref()?.render(part, signal, extras)
}

fn context(signal: &Signal, extras: &[(&str, Value)]) -> Value {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(signal) {
        for (key, value) in fields {
            if !value.is_null() {
                map.insert(key, Value::from_serialize(&value));
            }
        }
    }
    for (key, value) in extras {
        map.insert((*key).to_string(), value.clone());
    }
    Value::from(map)
}

/// Compiled templates from `(part, source)` pairs, for provider tests.
#[cfg(test)]
pub(crate) fn for_test(provider: &'static str, parts: &[(&str, &str)]) -> Option<Templates> {
    let config = parts
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Templates::compile(provider, &config).expect("valid templates")
}

fn config_err(msg: impl Into<String>) -> SignalError {
    SignalError::Config(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Severity, SignalInput};

    fn config(pairs: &[(&str, &str)]) -> TemplateConfig {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn signal() -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "load failed".into(),
            severity: Severity::Error,
            environment: Some("prod".into()),
            metadata: Some(serde_json::json!({ "dag_id": "orders", "tries": 3 })),
            ..Default::default()
        })
    }

    #[test]
    fn no_templates_compiles_to_none() {
        assert!(Templates::compile("slack", &TemplateConfig::new())
            .expect("ok")
            .is_none());
    }

    #[test]
    fn renders_signal_fields_metadata_and_extras() {
        let t = Templates::compile(
            "slack",
            &config(&[(
                "text",
                "{{ emoji }} [{{ environment|upper }}] {{ title }} ({{ metadata.dag_id }}, try {{ metadata.tries }})",
            )]),
        )
        .expect("ok")
        .expect("some");
        let text = t
            .render("text", &signal(), &[("emoji", Value::from(":x:"))])
            .expect("rendered");
        assert_eq!(text, ":x: [PROD] load failed (orders, try 3)");
    }

    #[test]
    fn unset_optional_fields_are_undefined() {
        let t = Templates::compile(
            "slack",
            &config(&[(
                "text",
                "{{ title }}{% if message %}: {{ message }}{% endif %}|{{ team }}|",
            )]),
        )
        .expect("ok")
        .expect("some");
        assert_eq!(
            t.render("text", &signal(), &[]).expect("rendered"),
            "load failed||"
        );
    }

    #[test]
    fn unknown_part_is_a_config_error() {
        let err = Templates::compile("slack", &config(&[("txt", "{{ title }}")]))
            .expect_err("unknown part");
        let msg = err.to_string();
        assert!(msg.contains("providers.slack.template.txt"), "{msg}");
        assert!(msg.contains("templatable: text"), "{msg}");
    }

    #[test]
    fn unknown_variable_is_a_config_error() {
        let err = Templates::compile("pagerduty", &config(&[("summary", "{{ tittle }}")]))
            .expect_err("typo");
        let msg = err.to_string();
        assert!(msg.contains("unknown variable tittle"), "{msg}");
    }

    #[test]
    fn provider_extras_are_not_shared() {
        // `emoji` is Slack's; Teams doesn't render Slack shortcodes.
        assert!(Templates::compile("teams", &config(&[("title", "{{ emoji }}")])).is_err());
    }

    #[test]
    fn syntax_error_is_a_config_error() {
        let err =
            Templates::compile("email", &config(&[("subject", "{{ title ")])).expect_err("syntax");
        assert!(err.to_string().contains("providers.email.template.subject"));
    }

    #[test]
    fn globals_and_loop_variables_are_allowed() {
        let t = Templates::compile(
            "email",
            &config(&[(
                "body",
                "{% for key, value in metadata|items %}{{ key }}={{ value }};{% endfor %}{% for i in range(2) %}{{ loop.index }}{% endfor %}",
            )]),
        )
        .expect("ok")
        .expect("some");
        assert_eq!(
            t.render("body", &signal(), &[]).expect("rendered"),
            "dag_id=orders;tries=3;12"
        );
    }

    #[test]
    fn runtime_failure_and_blank_output_fall_back() {
        let t = Templates::compile("console", &config(&[("text", "{{ metadata.dag_id + 1 }}")]))
            .expect("ok")
            .expect("some");
        assert!(t.render("text", &signal(), &[]).is_none());

        let blank = Templates::compile("console", &config(&[("text", "{{ message }}  \n")]))
            .expect("ok")
            .expect("some");
        assert!(blank.render("text", &signal(), &[]).is_none());
    }

    #[test]
    fn missing_part_is_none() {
        let t = Templates::compile("teams", &config(&[("title", "{{ title }}")]))
            .expect("ok")
            .expect("some");
        assert!(t.render("text", &signal(), &[]).is_none());
    }
}
