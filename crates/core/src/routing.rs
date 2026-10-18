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
        // A provider named by several matching routes is tried once, at
        // its first position, so a failing provider is not retried again
        // later in the same delivery.
        let mut matched: Vec<String> = Vec::new();
        for name in self
            .routes
            .iter()
            .filter(|r| r.match_rule.matches(signal))
            .flat_map(|r| r.providers.iter())
        {
            if !matched.contains(name) {
                matched.push(name.clone());
            }
        }

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

    fn route(
        severity: &[Severity],
        environment: Option<&str>,
        source: Option<&str>,
        providers: &[&str],
    ) -> Route {
        Route {
            match_rule: RouteMatch {
                severity: severity.to_vec(),
                environment: environment.map(str::to_string),
                source: source.map(str::to_string),
            },
            providers: providers.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn empty_routing_config_falls_back_to_console() {
        let config = RoutingConfig { routes: vec![] };
        assert_eq!(
            config.resolve(&sample_signal(Severity::Critical, "prod")),
            vec!["console".to_string()]
        );
    }

    #[test]
    fn empty_match_matches_every_signal() {
        let config = RoutingConfig {
            routes: vec![route(&[], None, None, &["webhook"])],
        };
        assert_eq!(
            config.resolve(&sample_signal(Severity::Info, "dev")),
            vec!["webhook".to_string()]
        );
    }

    #[test]
    fn every_field_in_a_match_must_hold() {
        let config = RoutingConfig {
            routes: vec![route(
                &[Severity::Error],
                Some("prod"),
                Some("airflow"),
                &["slack"],
            )],
        };
        assert_eq!(
            config.resolve(&sample_signal(Severity::Error, "prod")),
            vec!["slack".to_string()]
        );
        // Right severity and source, wrong environment.
        assert_eq!(
            config.resolve(&sample_signal(Severity::Error, "staging")),
            vec!["console".to_string()]
        );
        // Right severity and environment, wrong source.
        let mut other_source = sample_signal(Severity::Error, "prod");
        other_source.source = "jenkins".into();
        assert_eq!(config.resolve(&other_source), vec!["console".to_string()]);
    }

    #[test]
    fn environment_rule_does_not_match_signal_without_environment() {
        let config = RoutingConfig {
            routes: vec![route(&[], Some("prod"), None, &["slack"])],
        };
        let mut signal = sample_signal(Severity::Error, "prod");
        signal.environment = None;
        assert_eq!(config.resolve(&signal), vec!["console".to_string()]);
    }

    #[test]
    fn all_matching_routes_contribute_providers_in_config_order() {
        let config = RoutingConfig {
            routes: vec![
                route(&[Severity::Error], None, None, &["slack"]),
                route(&[Severity::Warning], None, None, &["console"]),
                route(&[], Some("prod"), None, &["webhook"]),
            ],
        };
        assert_eq!(
            config.resolve(&sample_signal(Severity::Error, "prod")),
            vec!["slack".to_string(), "webhook".to_string()]
        );
    }

    #[test]
    fn provider_named_by_several_routes_is_listed_once_at_its_first_position() {
        let config = RoutingConfig {
            routes: vec![
                route(&[Severity::Error], None, None, &["slack", "webhook"]),
                route(&[], Some("prod"), None, &["webhook", "slack"]),
            ],
        };
        assert_eq!(
            config.resolve(&sample_signal(Severity::Error, "prod")),
            vec!["slack".to_string(), "webhook".to_string()]
        );
    }
}
