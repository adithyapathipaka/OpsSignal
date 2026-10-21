//! Layered configs: `extends:` and a list of files in `$SIGNAL_CONFIG`.

use opssignal_core::config::Config;
use opssignal_core::source::{ConfigSource, LoadOptions};
use opssignal_core::SignalClient;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serves several files at once; see `config_source.rs` for why it has
/// its own runtime.
struct Server {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

impl Server {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("server runtime");
        let server = rt.block_on(MockServer::start());
        Server { rt, server }
    }

    /// Replaces everything served with `files`.
    fn serve(&self, files: &[(&str, u16, &str)]) {
        self.rt.block_on(async {
            self.server.reset().await;
            for (route, status, body) in files {
                Mock::given(method("GET"))
                    .and(path(*route))
                    .respond_with(ResponseTemplate::new(*status).set_body_string(*body))
                    .mount(&self.server)
                    .await;
            }
        });
    }

    fn url(&self, route: &str) -> String {
        format!("{}{route}", self.server.uri())
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "opssignal-layers-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let file = dir.join(name);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).expect("test fixture");
    }
    std::fs::write(&file, text).expect("test fixture");
    file
}

fn once(dir: &Path) -> LoadOptions {
    LoadOptions {
        refresh: None,
        cache_dir: Some(dir.join("cache")),
    }
}

/// The org-wide default: Slack for errors everywhere, and the sink.
fn default_yaml(dir: &Path) -> String {
    format!(
        "providers:\n  slack:\n    webhook_url: \"https://hooks.example.test/org\"\n  \
         webhook:\n    url: \"https://hooks.example.test/org-webhook\"\n\
         routes:\n  - match:\n      severity: [error, critical]\n    providers: [slack]\n\
         storage:\n  path: \"{}\"\n",
        dir.join("signals.db").display()
    )
}

fn team_route(source: &str, provider: &str) -> String {
    format!("  - match:\n      source: {source}\n    providers: [{provider}]\n")
}

fn route_sources(routes: &[opssignal_core::routing::Route]) -> Vec<Option<String>> {
    routes.iter().map(|r| r.match_rule.source.clone()).collect()
}

fn provider_names(client: &SignalClient) -> Vec<String> {
    let mut names: Vec<String> = client.pipeline().providers.keys().cloned().collect();
    names.sort_unstable();
    names
}

#[test]
fn a_team_file_inherits_the_default_and_overrides_by_name() {
    let dir = scratch("inherit");
    write(&dir, "default.yaml", &default_yaml(&dir));
    let team = write(
        &dir,
        "teams/payments.yaml",
        &format!(
            "extends: ../default.yaml\n\
             providers:\n  webhook:\n    url: \"https://hooks.example.test/payments\"\n\
             routes:\n{}",
            team_route("payments", "webhook")
        ),
    );

    let config = Config::from_file(&team).expect("layered config");

    let webhook = config.providers.webhook.expect("webhook");
    assert_eq!(webhook.url, "https://hooks.example.test/payments");
    let slack = config.providers.slack.expect("slack inherited");
    assert_eq!(
        slack.webhook_url.as_deref(),
        Some("https://hooks.example.test/org")
    );
    // The team's own route first, then the defaults it inherits.
    assert_eq!(
        route_sources(&config.routes),
        [Some("payments".to_string()), None]
    );
    assert_eq!(
        config.storage.path,
        dir.join("signals.db").display().to_string()
    );
}

#[test]
fn a_team_route_may_use_a_provider_only_its_parent_configures() {
    let dir = scratch("parent-provider");
    write(&dir, "default.yaml", &default_yaml(&dir));
    let team = write(
        &dir,
        "team.yaml",
        &format!(
            "extends: default.yaml\nroutes:\n{}",
            team_route("etl", "slack")
        ),
    );

    let config = Config::from_file(&team).expect("layered config");
    assert_eq!(config.routes.len(), 2);
}

#[test]
fn an_unknown_provider_is_reported_with_the_file_it_is_in() {
    let dir = scratch("unknown-provider");
    write(&dir, "default.yaml", "providers: {}\n");
    let team = write(
        &dir,
        "team.yaml",
        &format!(
            "extends: default.yaml\nroutes:\n{}",
            team_route("etl", "slack")
        ),
    );

    let err = Config::from_file(&team).expect_err("unknown provider");
    let msg = err.to_string();
    assert!(msg.contains("team.yaml: routes[0]"), "{msg}");
    assert!(msg.contains("'slack'"), "{msg}");
}

#[test]
fn a_list_of_files_merges_left_to_right_and_loads_shared_files_once() {
    let dir = scratch("list");
    write(&dir, "default.yaml", &default_yaml(&dir));
    // Alerts several teams want, kept in one place.
    write(
        &dir,
        "shared/db-alerts.yaml",
        &format!(
            "extends: ../default.yaml\nroutes:\n{}",
            team_route("postgres", "slack")
        ),
    );
    let a = write(
        &dir,
        "teams/a.yaml",
        &format!(
            "extends: [../default.yaml, ../shared/db-alerts.yaml]\nroutes:\n{}",
            team_route("team-a", "webhook")
        ),
    );
    let b = write(
        &dir,
        "teams/b.yaml",
        &format!(
            "extends: ../shared/db-alerts.yaml\nroutes:\n{}",
            team_route("team-b", "slack")
        ),
    );
    let raw = format!("{}, {}", a.display(), b.display());
    let sources = ConfigSource::parse_list(&raw).expect("sources");

    let client = SignalClient::from_sources(&sources, &once(&dir)).expect("client");

    // Each file once, at its first position: default, shared, a, b.
    let routes = &client.pipeline().routing.routes;
    assert_eq!(
        route_sources(routes),
        [
            Some("team-b".to_string()),
            Some("team-a".to_string()),
            Some("postgres".to_string()),
            None
        ]
    );
}

#[test]
fn extends_cycles_are_rejected() {
    let dir = scratch("cycle");
    let a = write(&dir, "a.yaml", "extends: b.yaml\n");
    write(&dir, "b.yaml", "extends: [a.yaml]\n");

    let err = Config::from_file(&a).expect_err("cycle");
    let msg = err.to_string();
    assert!(msg.contains("cycle"), "{msg}");
    assert!(
        msg.contains("a.yaml -> ") && msg.contains("b.yaml -> "),
        "{msg}"
    );
}

#[test]
fn overly_deep_chains_are_rejected() {
    let dir = scratch("depth");
    for i in 0..10 {
        write(
            &dir,
            &format!("{i}.yaml"),
            &format!("extends: {}.yaml\n", i + 1),
        );
    }
    write(&dir, "10.yaml", "");

    let err = Config::from_file(&dir.join("0.yaml")).expect_err("too deep");
    assert!(err.to_string().contains("deeper than 8"), "{err}");
}

#[test]
fn extends_must_be_a_path_or_a_list_of_paths() {
    let dir = scratch("bad-extends");
    let file = write(&dir, "team.yaml", "extends: {file: default.yaml}\n");
    assert!(Config::from_file(&file).is_err());
}

#[test]
fn a_missing_parent_file_stops_startup() {
    let dir = scratch("missing-parent");
    let team = write(&dir, "team.yaml", "extends: nope.yaml\n");
    let err = SignalClient::from_source(&ConfigSource::File(team), &once(&dir))
        .err()
        .expect("missing parent");
    assert!(err.to_string().contains("nope.yaml"), "{err}");
}

#[test]
fn from_yaml_str_cannot_resolve_extends() {
    let err = Config::from_yaml_str("extends: default.yaml\n").expect_err("no location");
    assert!(err.to_string().contains("Config::from_file"), "{err}");
}

#[test]
fn a_change_to_the_default_reloads_every_team() {
    let dir = scratch("reload");
    let default = write(&dir, "default.yaml", "providers: {}\n");
    let team = write(&dir, "team.yaml", "extends: default.yaml\n");
    let opts = LoadOptions {
        refresh: Some(Duration::from_millis(30)),
        cache_dir: Some(dir.join("cache")),
    };
    let client = SignalClient::from_source(&ConfigSource::File(team), &opts).expect("client");
    assert_eq!(provider_names(&client), ["console"]);

    std::fs::write(&default, default_yaml(&dir)).expect("test fixture");

    let deadline = Instant::now() + Duration::from_secs(5);
    while provider_names(&client) != ["console", "slack", "webhook"] {
        assert!(Instant::now() < deadline, "parent edit applied");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_remote_team_file_extends_a_relative_remote_default_and_caches_both() {
    let dir = scratch("remote");
    let server = Server::start();
    let default = default_yaml(&dir);
    let team = format!(
        "extends: ../default.yaml\nroutes:\n{}",
        team_route("payments", "webhook")
    );
    server.serve(&[
        ("/config/default.yaml", 200, &default),
        ("/config/teams/payments.yaml", 200, &team),
    ]);
    let source = ConfigSource::parse(&server.url("/config/teams/payments.yaml")).expect("source");

    let client = SignalClient::from_source(&source, &once(&dir)).expect("client");
    assert_eq!(provider_names(&client), ["console", "slack", "webhook"]);
    drop(client);

    // The default goes away; the next start uses its cached copy.
    server.serve(&[
        ("/config/default.yaml", 503, ""),
        ("/config/teams/payments.yaml", 200, &team),
    ]);
    let client = SignalClient::from_source(&source, &once(&dir)).expect("client");
    assert_eq!(provider_names(&client), ["console", "slack", "webhook"]);
}

#[test]
fn fresh_remote_files_that_do_not_merge_fall_back_to_the_cached_chain() {
    let dir = scratch("remote-merge");
    let server = Server::start();
    let default = default_yaml(&dir);
    let team = format!(
        "extends: default.yaml\nroutes:\n{}",
        team_route("etl", "slack")
    );
    server.serve(&[("/default.yaml", 200, &default), ("/team.yaml", 200, &team)]);
    let source = ConfigSource::parse(&server.url("/team.yaml")).expect("source");
    drop(SignalClient::from_source(&source, &once(&dir)).expect("client"));

    // Someone drops slack from the default while a team still routes to it.
    server.serve(&[
        ("/default.yaml", 200, "providers: {}\n"),
        ("/team.yaml", 200, &team),
    ]);
    let client = SignalClient::from_source(&source, &once(&dir)).expect("client");
    assert_eq!(provider_names(&client), ["console", "slack", "webhook"]);
}

#[test]
fn a_remote_file_cannot_extend_a_local_path() {
    let dir = scratch("remote-local");
    let server = Server::start();
    server.serve(&[("/team.yaml", 200, "extends: /etc/opssignal/default.yaml\n")]);
    let source = ConfigSource::parse(&server.url("/team.yaml")).expect("source");

    let err = SignalClient::from_source(&source, &once(&dir))
        .err()
        .expect("rejected");
    assert!(
        err.to_string().contains("cannot extend the local file"),
        "{err}"
    );
}
