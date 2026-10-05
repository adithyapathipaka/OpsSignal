#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use opssignal_core::config::Config;
use opssignal_core::delivery::DeliveryStatus;
use opssignal_core::signal::{Severity, SignalInput};
use opssignal_core::SignalClient;
use std::path::PathBuf;
use std::time::Duration;

const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "signal", version, about = "Operational signal SDK CLI")]
struct Cli {
    /// Path to signal.yaml. Defaults to $SIGNAL_CONFIG, then ./signal.yaml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

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

            let config = match &cli.config {
                Some(path) => Config::from_file(path),
                None => Config::discover(),
            };
            let client = match config.and_then(|c| SignalClient::from_config(&c)) {
                Ok(client) => client,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };

            // Exit codes: 0 delivered, 1 delivery failed, 2 bad input or config.
            match client.notify_sync(input, NOTIFY_TIMEOUT) {
                Ok(result) if result.status == DeliveryStatus::Delivered => {}
                Ok(result) => {
                    eprintln!(
                        "error: signal not delivered: {}",
                        result.error.unwrap_or_else(|| result.status.to_string())
                    );
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
