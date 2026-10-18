//! Runs the built `opssignal` binary and checks its exit codes:
//! 0 delivered, 1 delivery failed, 2 bad input or config.

use std::path::PathBuf;
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "opssignal-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    /// Writes a signal.yaml whose sink lives in this directory.
    fn config(&self, extra: &str) -> PathBuf {
        let path = self.0.join("signal.yaml");
        let db = self.0.join("signals.db");
        std::fs::write(
            &path,
            format!("{extra}storage:\n  path: \"{}\"\n", db.display()),
        )
        .expect("write config");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str], dir: &TempDir) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opssignal"))
        .args(args)
        .current_dir(&dir.0)
        .env_remove("SIGNAL_CONFIG")
        .output()
        .expect("run opssignal")
}

fn notify_args<'a>(config: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "--config",
        config,
        "notify",
        "--source",
        "ci",
        "--event-type",
        "build_failed",
        "--title",
        "main is red",
    ];
    args.extend_from_slice(extra);
    args
}

fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

#[test]
fn delivered_signal_exits_zero_and_prints_to_console() {
    let dir = TempDir::new();
    let config = dir.config("");
    let out = run(
        &notify_args(
            config.to_str().expect("utf-8 path"),
            &["--severity", "ERROR"],
        ),
        &dir,
    );

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[signal:ci] error build_failed"),
        "stdout: {stdout}"
    );
    assert!(dir.0.join("signals.db").is_file());
}

#[test]
fn zero_config_discovers_signal_yaml_in_the_working_directory() {
    let dir = TempDir::new();
    dir.config("");
    let out = run(
        &[
            "notify",
            "--source",
            "ci",
            "--event-type",
            "e",
            "--title",
            "t",
        ],
        &dir,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.0.join("signals.db").is_file());
}

#[test]
fn failed_delivery_exits_one() {
    let dir = TempDir::new();
    let config = dir.config(&format!(
        "providers:\n  webhook:\n    url: \"http://127.0.0.1:{}/hook\"\nroutes:\n  - match: {{}}\n    providers: [webhook]\n",
        closed_port()
    ));
    let out = run(
        &notify_args(config.to_str().expect("utf-8 path"), &[]),
        &dir,
    );

    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("signal not delivered"));
}

#[test]
fn unknown_severity_exits_two() {
    let dir = TempDir::new();
    let config = dir.config("");
    let out = run(
        &notify_args(
            config.to_str().expect("utf-8 path"),
            &["--severity", "bogus"],
        ),
        &dir,
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown severity"));
}

#[test]
fn blank_title_exits_two() {
    let dir = TempDir::new();
    let config = dir.config("");
    let out = run(
        &[
            "--config",
            config.to_str().expect("utf-8 path"),
            "notify",
            "--source",
            "ci",
            "--event-type",
            "e",
            "--title",
            " ",
        ],
        &dir,
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("title must not be empty"));
}

#[test]
fn missing_config_file_exits_two() {
    let dir = TempDir::new();
    let missing = dir.0.join("nope.yaml");
    let out = run(
        &notify_args(missing.to_str().expect("utf-8 path"), &[]),
        &dir,
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nope.yaml"));
}

#[test]
fn invalid_config_exits_two() {
    let dir = TempDir::new();
    let config = dir.config("routes:\n  - match: {}\n    providers: [slack]\n");
    let out = run(
        &notify_args(config.to_str().expect("utf-8 path"), &[]),
        &dir,
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("'slack'"));
}

#[test]
fn missing_required_argument_is_a_usage_error() {
    let dir = TempDir::new();
    let out = run(&["notify", "--source", "ci"], &dir);

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--event-type"));
}
