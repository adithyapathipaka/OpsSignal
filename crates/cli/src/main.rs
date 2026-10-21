#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use opssignal_core::config::Config;
use opssignal_core::delivery::DeliveryStatus;
use opssignal_core::signal::{Severity, SignalInput};
use opssignal_core::source::{ConfigSource, LoadOptions};
use opssignal_core::SignalClient;
use std::time::Duration;

const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "signal", version, about = "Operational signal SDK CLI")]
struct Cli {
    /// Path or URL (https://, s3://, gs://, az://) of signal.yaml, or several
    /// comma separated, lowest priority first. Defaults to $SIGNAL_CONFIG,
    /// then ./signal.yaml.
    #[arg(long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a signal from the shell, CI, or a cron job.
    Notify {
        #[arg(long)]
        source: String,
        #[arg(long = "event-type")]
        event_type: String,
        #[arg(long, default_value = "info")]
        severity: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        environment: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Notify {
            source,
            event_type,
            severity,
            title,
            message,
            environment,
        } => {
            let severity = match severity.parse::<Severity>() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };

            let input = SignalInput {
                source,
                event_type,
                severity,
                title,
                message,
                environment,
                ..Default::default()
            };

            let client = match build_client(cli.config.as_deref()) {
                Ok(client) => client,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };

            // Exit codes: 0 delivered, 1 delivery failed or only partly
            // delivered, 2 bad input or config.
            match client.notify_sync(input, NOTIFY_TIMEOUT) {
                Ok(result) if result.status == DeliveryStatus::Delivered => {}
                Ok(result) => {
                    let what = match result.status {
                        DeliveryStatus::Partial => "signal only partly delivered",
                        _ => "signal not delivered",
                    };
                    eprintln!(
                        "error: {what}: {}",
                        result.error.unwrap_or_else(|| result.status.to_string())
                    );
                    for outcome in &result.deliveries {
                        eprintln!("  {}: {}", outcome.provider, outcome.status);
                    }
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            }
        }
    }
}

/// A one-shot command loads the config once; there is nothing to reload.
fn build_client(config: Option<&str>) -> Result<SignalClient, opssignal_core::signal::SignalError> {
    let sources = match config {
        Some(raw) => Some(ConfigSource::parse_list(raw)?),
        None => ConfigSource::discover()?,
    };
    let opts = LoadOptions {
        refresh: None,
        ..LoadOptions::from_env()?
    };
    match sources {
        Some(sources) => SignalClient::from_sources(&sources, &opts),
        None => SignalClient::from_config(&Config::default()),
    }
}
