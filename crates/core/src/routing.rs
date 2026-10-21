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

/// How a route delivers to the providers it lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryMode {
    /// Try the providers in order and stop at the first that succeeds,
    /// so later providers are fallbacks.
    #[default]
    Failover,
    /// Send to every provider at once. Each one is retried on its own,
    /// so one that fails never causes a resend to one that succeeded.
    Fanout,
}

// Unknown keys are rejected so a typo such as `mod: fanout` fails at
// load time instead of quietly leaving the route on failover.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    pub providers: Vec<String>,
    #[serde(default)]
    pub mode: DeliveryMode,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingConfig {
    pub routes: Vec<Route>,
}

/// Where one signal goes. Every target is delivered concurrently and
/// independently; within a target the providers are tried in order
/// until one succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    pub targets: Vec<Vec<String>>,
}

impl DeliveryPlan {
    /// Every provider of the plan, in plan order.
    pub fn providers(&self) -> impl Iterator<Item = &String> {
        self.targets.iter().flatten()
    }
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
    /// Returns the provider names a signal should be delivered to, in
    /// the order of [`RoutingConfig::plan`].
    pub fn resolve(&self, signal: &Signal) -> Vec<String> {
        self.plan(signal).providers().cloned().collect()
    }

    /// Groups the matching routes' providers into delivery targets.
    ///
    /// Each provider of a `fanout` route is a target of its own. The
    /// providers of every matching `failover` route share one target,
    /// placed where the first of those routes is, so a config without
    /// `fanout` delivers exactly as it always has: one ordered list that
    /// stops at the first success.
    ///
    /// CRITICAL INVARIANT: if no route matches, this MUST fall back to
    /// "console" rather than returning an empty plan. A signal that
    /// matches nothing must never silently disappear — that is the one
    /// failure mode that destroys trust in an alerting tool. See the
    /// architecture review: "unmatched signals always hit console at
    /// minimum."
    pub fn plan(&self, signal: &Signal) -> DeliveryPlan {
        // A provider named by several matching routes is used once, at
        // its first position, so it never gets the same signal twice and
        // a failing provider is not retried again later in the same
        // delivery.
        let mut seen: Vec<&String> = Vec::new();
        let mut targets: Vec<Vec<String>> = Vec::new();
        let mut failover: Option<usize> = None;
        for route in self.routes.iter().filter(|r| r.match_rule.matches(signal)) {
            for name in &route.providers {
                if seen.contains(&name) {
                    continue;
                }
                seen.push(name);
                match (route.mode, failover) {
                    (DeliveryMode::Fanout, _) => targets.push(vec![name.clone()]),
                    (DeliveryMode::Failover, Some(i)) => targets[i].push(name.clone()),
                    (DeliveryMode::Failover, None) => {
                        failover = Some(targets.len());
                        targets.push(vec![name.clone()]);
                    }
                }
            }
        }

        if targets.is_empty() {
            targets.push(vec!["console".to_string()]);
        }
        DeliveryPlan { targets }
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
                mode: DeliveryMode::Failover,
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
                mode: DeliveryMode::Failover,
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
            mode: DeliveryMode::Failover,
        }
    }

    fn fanout(route: Route) -> Route {
        Route {
            mode: DeliveryMode::Fanout,
            ..route
        }
    }

    fn targets(config: &RoutingConfig, signal: &Signal) -> Vec<Vec<String>> {
        config.plan(signal).targets
    }

    fn names(targets: &[&[&str]]) -> Vec<Vec<String>> {
        targets
            .iter()
            .map(|t| t.iter().map(|p| p.to_string()).collect())
            .collect()
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

    #[test]
    fn failover_routes_share_one_target() {
        let config = RoutingConfig {
            routes: vec![
                route(&[Severity::Error], None, None, &["pagerduty", "teams"]),
                route(&[], Some("prod"), None, &["slack"]),
            ],
        };
        assert_eq!(
            targets(&config, &sample_signal(Severity::Error, "prod")),
            names(&[&["pagerduty", "teams", "slack"]])
        );
    }

    #[test]
    fn fanout_route_gives_each_provider_its_own_target() {
        let config = RoutingConfig {
            routes: vec![fanout(route(&[], None, None, &["slack", "pagerduty"]))],
        };
        assert_eq!(
            targets(&config, &sample_signal(Severity::Error, "prod")),
            names(&[&["slack"], &["pagerduty"]])
        );
    }

    #[test]
    fn mixed_modes_keep_the_failover_chain_where_its_first_route_is() {
        let config = RoutingConfig {
            routes: vec![
                route(&[Severity::Error], None, None, &["teams"]),
                fanout(route(&[], Some("prod"), None, &["slack", "pagerduty"])),
                route(&[], None, None, &["email"]),
            ],
        };
        assert_eq!(
            targets(&config, &sample_signal(Severity::Error, "prod")),
            names(&[&["teams", "email"], &["slack"], &["pagerduty"]])
        );
    }

    #[test]
    fn provider_in_both_modes_is_used_once_at_its_first_position() {
        let config = RoutingConfig {
            routes: vec![
                fanout(route(&[], None, None, &["slack", "pagerduty"])),
                route(&[], None, None, &["pagerduty", "teams"]),
            ],
        };
        assert_eq!(
            targets(&config, &sample_signal(Severity::Error, "prod")),
            names(&[&["slack"], &["pagerduty"], &["teams"]])
        );
    }

    #[test]
    fn unmatched_fanout_route_still_falls_back_to_console() {
        let config = RoutingConfig {
            routes: vec![fanout(route(&[Severity::Critical], None, None, &["slack"]))],
        };
        assert_eq!(
            targets(&config, &sample_signal(Severity::Info, "dev")),
            names(&[&["console"]])
        );
    }

    #[test]
    fn mode_parses_and_defaults_to_failover() {
        let routes: Vec<Route> = serde_yaml::from_str(
            "- match: {}\n  providers: [slack]\n- match: {}\n  providers: [slack]\n  mode: fanout\n",
        )
        .expect("routes");
        assert_eq!(routes[0].mode, DeliveryMode::Failover);
        assert_eq!(routes[1].mode, DeliveryMode::Fanout);
    }

    #[test]
    fn unknown_mode_or_route_key_is_rejected() {
        let bad_mode: Result<Vec<Route>, _> =
            serde_yaml::from_str("- match: {}\n  providers: [slack]\n  mode: broadcast\n");
        assert!(bad_mode.is_err());
        let typo: Result<Vec<Route>, _> =
            serde_yaml::from_str("- match: {}\n  providers: [slack]\n  mod: fanout\n");
        assert!(typo.is_err());
    }
}
