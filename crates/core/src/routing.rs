use crate::signal::{Severity, Signal};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// Unknown keys are rejected: a typo such as `enviroment: prod` would
// otherwise be ignored and leave a match that every signal passes.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
    /// Sends a signal through this route at most once per window for
    /// each dedup key; repeats inside the window are suppressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup: Option<Window>,
    /// Caps how many signals this route sends, whatever their key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Holds matching signals and sends one digest for each distinct
    /// combination of these fields: signal fields such as `source` or
    /// `run_id`, or `metadata.<key>`. See [`GROUP_FIELDS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<Vec<String>>,
    /// How long a group stays open after its first signal before the
    /// digest is sent. Defaults to [`DEFAULT_GROUP_WAIT`] when only
    /// `group_by` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_wait: Option<Window>,
}

/// How long a group waits when a route sets `group_by` but no
/// `group_wait`.
pub const DEFAULT_GROUP_WAIT: Duration = Duration::from_secs(30);

/// The signal fields a route can `group_by`, besides `metadata.<key>`.
pub const GROUP_FIELDS: &[&str] = &[
    "source",
    "service",
    "component",
    "event_type",
    "severity",
    "title",
    "environment",
    "team",
    "run_id",
    "correlation_id",
    "dedup_key",
];

impl Route {
    /// Whether the route keeps any dedup or rate-limit state.
    pub fn is_throttled(&self) -> bool {
        self.dedup.is_some() || self.rate_limit.is_some()
    }

    /// Whether the route sends digests instead of single signals.
    pub fn is_grouped(&self) -> bool {
        self.group_by.is_some() || self.group_wait.is_some()
    }

    /// How long this route's groups stay open, if it groups.
    pub fn group_window(&self) -> Option<Duration> {
        self.is_grouped()
            .then(|| self.group_wait.map_or(DEFAULT_GROUP_WAIT, |w| w.0))
    }

    /// Whether a signal must be checked against the route's history
    /// while the plan is built. A grouped route checks only `dedup`
    /// there; its `rate_limit` counts digests, so it is checked when
    /// one is sent.
    fn gates_signals(&self) -> bool {
        if self.is_grouped() {
            self.dedup.is_some()
        } else {
            self.is_throttled()
        }
    }

    /// This route's providers as delivery targets, on their own: one
    /// ordered chain for `failover`, one target per provider for
    /// `fanout`. Used to send a digest.
    pub fn targets(&self) -> Vec<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        for name in &self.providers {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        match self.mode {
            DeliveryMode::Failover => vec![names],
            DeliveryMode::Fanout => names.into_iter().map(|n| vec![n]).collect(),
        }
    }

    /// The route's providers, as `[slack, pagerduty]`, for messages.
    pub fn describe(&self) -> String {
        format!("[{}]", self.providers.join(", "))
    }
}

/// At most `max` signals per `per`, counted over a sliding window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub max: u32,
    pub per: Window,
}

/// A length of time written as a number and a unit: `30s`, `10m`,
/// `2h` or `1d`. A bare number is seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(pub Duration);

impl std::str::FromStr for Window {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: u64 = num
            .parse()
            .map_err(|_| format!("invalid duration {s:?}; expected e.g. 30s, 10m, 2h or 1d"))?;
        let secs = match unit.trim() {
            "" | "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            other => {
                return Err(format!(
                    "invalid duration unit {other:?} in {s:?}; expected s, m, h or d"
                ))
            }
        };
        Ok(Window(Duration::from_secs(n.saturating_mul(secs))))
    }
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self.0.as_secs();
        let (n, unit) = [(86400, "d"), (3600, "h"), (60, "m")]
            .into_iter()
            .find(|(size, _)| secs > 0 && secs.is_multiple_of(*size))
            .map(|(size, unit)| (secs / size, unit))
            .unwrap_or((secs, "s"));
        write!(f, "{n}{unit}")
    }
}

impl<'de> Deserialize<'de> for Window {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Secs(n) => Ok(Window(Duration::from_secs(n))),
            Raw::Text(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for Window {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
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
    /// Why matching routes were held back by their `dedup` or
    /// `rate_limit`, one line per route. Only [`RoutingConfig::plan_with`]
    /// fills it.
    pub suppressed: Vec<String>,
    /// Matching routes that hold the signal for a digest instead of
    /// sending it now: the route's index in [`RoutingConfig::routes`]
    /// and a line saying so. Their providers are not in `targets`.
    pub grouped: Vec<(usize, String)>,
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
        self.plan_with(signal, |_| Ok(()))
    }

    /// [`RoutingConfig::plan`] that first asks `admit` about each
    /// matching route that sets `dedup` or `rate_limit`. A route it
    /// turns down (with the reason) adds no providers. A route that
    /// sets `group_by` or `group_wait` adds no providers either: it is
    /// listed in `grouped`, and only its `dedup` is asked about.
    ///
    /// Unlike `plan`, this can return no targets: when routes matched
    /// and every one of them was suppressed. That is a decision the
    /// config asked for, not a signal that matched nothing, so it does
    /// not fall back to console; `suppressed` says why.
    pub fn plan_with<F>(&self, signal: &Signal, mut admit: F) -> DeliveryPlan
    where
        F: FnMut(&Route) -> Result<(), String>,
    {
        // A provider named by several matching routes is used once, at
        // its first position, so it never gets the same signal twice and
        // a failing provider is not retried again later in the same
        // delivery.
        let mut seen: Vec<&String> = Vec::new();
        let mut targets: Vec<Vec<String>> = Vec::new();
        let mut failover: Option<usize> = None;
        let mut suppressed = Vec::new();
        let mut grouped = Vec::new();
        for (i, route) in self.routes.iter().enumerate() {
            if !route.match_rule.matches(signal) {
                continue;
            }
            if route.gates_signals() {
                if let Err(reason) = admit(route) {
                    suppressed.push(format!("route {}: {reason}", route.describe()));
                    continue;
                }
            }
            if let Some(wait) = route.group_window() {
                grouped.push((
                    i,
                    format!(
                        "route {}: held for a digest (group_wait {})",
                        route.describe(),
                        Window(wait)
                    ),
                ));
                continue;
            }
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

        if targets.is_empty() && suppressed.is_empty() && grouped.is_empty() {
            targets.push(vec!["console".to_string()]);
        }
        DeliveryPlan {
            targets,
            suppressed,
            grouped,
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
                mode: DeliveryMode::Failover,
                dedup: None,
                rate_limit: None,
                group_by: None,
                group_wait: None,
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
                dedup: None,
                rate_limit: None,
                group_by: None,
                group_wait: None,
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
            dedup: None,
            rate_limit: None,
            group_by: None,
            group_wait: None,
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

    #[test]
    fn plan_with_skips_routes_the_gate_turns_down() {
        let mut limited = fanout(route(&[], None, None, &["pagerduty", "slack"]));
        limited.dedup = Some("10m".parse().expect("window"));
        let config = RoutingConfig {
            routes: vec![limited, route(&[], None, None, &["teams", "slack"])],
        };
        let plan = config.plan_with(&sample_signal(Severity::Error, "prod"), |_| {
            Err("duplicate".to_string())
        });
        // slack is still sent, now from the failover route.
        assert_eq!(plan.targets, names(&[&["teams", "slack"]]));
        assert_eq!(
            plan.suppressed,
            vec!["route [pagerduty, slack]: duplicate".to_string()]
        );
    }

    #[test]
    fn gate_is_asked_only_about_throttled_routes() {
        let mut limited = route(&[], None, None, &["pagerduty"]);
        limited.rate_limit = Some(RateLimit {
            max: 1,
            per: "1m".parse().expect("window"),
        });
        let config = RoutingConfig {
            routes: vec![route(&[], None, None, &["slack"]), limited],
        };
        let mut asked = Vec::new();
        let plan = config.plan_with(&sample_signal(Severity::Error, "prod"), |r| {
            asked.push(r.providers.clone());
            Ok(())
        });
        assert_eq!(asked, vec![vec!["pagerduty".to_string()]]);
        assert_eq!(plan.targets, names(&[&["slack", "pagerduty"]]));
    }

    #[test]
    fn every_matching_route_suppressed_gives_an_empty_plan_not_console() {
        let mut limited = route(&[Severity::Error], None, None, &["pagerduty"]);
        limited.dedup = Some("1m".parse().expect("window"));
        let config = RoutingConfig {
            routes: vec![limited],
        };
        let plan = config.plan_with(&sample_signal(Severity::Error, "prod"), |_| {
            Err("rate limited".to_string())
        });
        assert!(plan.targets.is_empty());
        assert_eq!(plan.suppressed.len(), 1);
        // A signal no route matches still goes to console.
        let unmatched = config.plan_with(&sample_signal(Severity::Info, "prod"), |_| {
            Err("never asked".to_string())
        });
        assert_eq!(unmatched.targets, names(&[&["console"]]));
        assert!(unmatched.suppressed.is_empty());
    }

    #[test]
    fn grouped_route_is_held_not_planned_and_does_not_fall_back_to_console() {
        let mut held = route(&[], None, None, &["slack"]);
        held.group_by = Some(vec!["run_id".into()]);
        let config = RoutingConfig {
            routes: vec![
                route(&[Severity::Critical], None, None, &["pagerduty"]),
                held,
            ],
        };
        let plan = config.plan(&sample_signal(Severity::Error, "prod"));
        assert!(plan.targets.is_empty());
        assert_eq!(
            plan.grouped,
            vec![(
                1,
                "route [slack]: held for a digest (group_wait 30s)".to_string()
            )]
        );
        let critical = config.plan(&sample_signal(Severity::Critical, "prod"));
        assert_eq!(critical.targets, names(&[&["pagerduty"]]));
        assert_eq!(critical.grouped.len(), 1);
    }

    #[test]
    fn grouped_route_asks_the_gate_only_for_dedup() {
        let mut limited = route(&[], None, None, &["slack"]);
        limited.group_wait = Some("1m".parse().expect("window"));
        limited.rate_limit = Some(RateLimit {
            max: 1,
            per: "1m".parse().expect("window"),
        });
        let config = RoutingConfig {
            routes: vec![limited.clone()],
        };
        let plan = config.plan_with(&sample_signal(Severity::Error, "prod"), |_| {
            Err("never asked".to_string())
        });
        assert_eq!(plan.grouped.len(), 1);

        limited.dedup = Some("1m".parse().expect("window"));
        let config = RoutingConfig {
            routes: vec![limited],
        };
        let plan = config.plan_with(&sample_signal(Severity::Error, "prod"), |_| {
            Err("duplicate".to_string())
        });
        assert!(plan.grouped.is_empty());
        assert_eq!(plan.suppressed.len(), 1);
    }

    #[test]
    fn route_targets_follow_its_mode() {
        let failover = route(&[], None, None, &["teams", "slack", "teams"]);
        assert_eq!(failover.targets(), names(&[&["teams", "slack"]]));
        assert_eq!(fanout(failover).targets(), names(&[&["teams"], &["slack"]]));
    }

    #[test]
    fn windows_parse_and_print() {
        for (text, secs, shown) in [
            ("30s", 30, "30s"),
            ("90", 90, "90s"),
            ("10m", 600, "10m"),
            ("120m", 7200, "2h"),
            ("1d", 86400, "1d"),
        ] {
            let w: Window = text.parse().expect(text);
            assert_eq!(w.0, Duration::from_secs(secs));
            assert_eq!(w.to_string(), shown);
        }
        for bad in ["", "m", "10 minutes", "-5m", "1.5h"] {
            assert!(bad.parse::<Window>().is_err(), "{bad}");
        }
    }

    #[test]
    fn unknown_match_key_is_rejected() {
        let typo: Result<Vec<Route>, _> =
            serde_yaml::from_str("- match: {enviroment: prod}\n  providers: [slack]\n");
        assert!(typo.is_err());
    }
}
