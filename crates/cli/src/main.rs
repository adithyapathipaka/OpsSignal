#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use opssignal_core::signal::{Severity, SignalInput};

#[derive(Parser)]
#[command(name = "signal", version, about = "Operational signal SDK CLI")]
struct Cli {
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

            let _input = SignalInput {
                source,
                event_type,
                severity,
                title,
                message,
                environment,
                ..Default::default()
            };

            // TODO(v0.1): construct a SignalClient from the local
            // config file (./signal.yaml or $SIGNAL_CONFIG) and call
            // notify_sync with a sane default timeout (e.g. 5s) so CLI
            // invocations from CI exit with a clear status code.
            eprintln!(
                "signal-cli is scaffolded but not yet wired to SignalClient — see TODO in main.rs"
            );
            std::process::exit(1);
        }
    }
}
