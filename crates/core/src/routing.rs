use crate::signal::{Severity, Signal};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteMatch {
    #[serde(default)]
    pub severity: Vec<Severity>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Route {
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingConfig {
    pub routes: Vec<Route>,
}

impl RouteMatch {
    fn matches(&self, signal: &Signal) -> bool {
        if !self.severity.is_empty() && !self.severity.contains(&signal.severity) {
            return false;
        }
        if let Some(env) = &self.environment {
            if signal.environment.as_deref() != Some(env.as_str()) {
                return false;
            }
        }
        if let Some(src) = &self.source {
            if signal.source != *src {
                return false;
            }
        }
        true
    }
}

impl RoutingConfig {
    /// Returns the provider names a signal should be delivered to.
    ///
    /// CRITICAL INVARIANT: if no route matches, this MUST fall back to
    /// "console" rather than returning an empty list. A signal that
    /// matches nothing must never silently disappear — that is the one
    /// failure mode that destroys trust in an alerting tool. See the
    /// architecture review: "unmatched signals always hit console at
    /// minimum."
    pub fn resolve(&self, signal: &Signal) -> Vec<String> {
        let mut matched: Vec<String> = self
            .routes
            .iter()
            .filter(|r| r.match_rule.matches(signal))
            .flat_map(|r| r.providers.clone())
            .collect();

        matched.dedup();

        if matched.is_empty() {
            vec!["console".to_string()]
        } else {
            matched
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SignalInput;

    fn sample_signal(severity: Severity, env: &str) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "test".into(),
            severity,
            environment: Some(env.to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn unmatched_signal_falls_back_to_console() {
        let config = RoutingConfig {
            routes: vec![Route {
                match_rule: RouteMatch {
                    severity: vec![Severity::Critical],
                    environment: None,
                    source: None,
                },
                providers: vec!["slack".into()],
            }],
        };
        let signal = sample_signal(Severity::Info, "dev");
        assert_eq!(config.resolve(&signal), vec!["console".to_string()]);
    }

    #[test]
    fn matched_signal_routes_to_configured_provider() {
        let config = RoutingConfig {
            routes: vec![Route {
                match_rule: RouteMatch {
                    severity: vec![Severity::Critical, Severity::Error],
                    environment: Some("prod".into()),
                    source: None,
                },
                providers: vec!["slack".into()],
            }],
        };
        let signal = sample_signal(Severity::Error, "prod");
        assert_eq!(config.resolve(&signal), vec!["slack".to_string()]);
    }
}
