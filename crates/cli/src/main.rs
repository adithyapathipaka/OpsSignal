#![forbid(unsafe_code)]

use clap::{Args, Parser, Subcommand};
use opssignal_core::config::Config;
use opssignal_core::delivery::DeliveryStatus;
use opssignal_core::signal::{Severity, Signal, SignalError, SignalInput};
use opssignal_core::source::{ConfigSource, LoadOptions};
use opssignal_core::{Pipeline, SignalClient};
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
    Notify(SignalArgs),
    /// Print what each provider would be sent for a signal, as JSON,
    /// without sending or recording it. For checking message templates.
    Preview {
        #[command(flatten)]
        signal: SignalArgs,
        /// Show this provider's payload even if no route picks it.
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Args)]
struct SignalArgs {
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
    /// Extra fields for templates, as a JSON object.
    #[arg(long)]
    metadata: Option<String>,
}

impl SignalArgs {
    fn into_input(self) -> Result<SignalInput, String> {
        let severity = self.severity.parse::<Severity>()?;
        let metadata = match self.metadata {
            Some(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(value @ serde_json::Value::Object(_)) => Some(value),
                Ok(_) => return Err("--metadata must be a JSON object".into()),
                Err(e) => return Err(format!("--metadata is not valid JSON: {e}")),
            },
            None => None,
        };
        Ok(SignalInput {
            source: self.source,
            event_type: self.event_type,
            severity,
            title: self.title,
            message: self.message,
            environment: self.environment,
            metadata,
            ..Default::default()
        })
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Preview { signal, provider } => {
            let input = signal.into_input().unwrap_or_else(|e| fail(&e));
            let config = load_config(cli.config.as_deref()).unwrap_or_else(|e| fail(&e));
            let signal = Signal::from_input(input);
            let previews = Pipeline::from_config(&config)
                .preview(&signal, provider.as_deref())
                .unwrap_or_else(|e| fail(&e));
            let out: Vec<serde_json::Value> = previews
                .into_iter()
                .map(|(provider, payload)| serde_json::json!({ "provider": provider, "payload": payload }))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&out).unwrap_or_else(|e| fail(&e))
            );
        }
        Commands::Notify(signal) => {
            let input = signal.into_input().unwrap_or_else(|e| fail(&e));

            let client = build_client(cli.config.as_deref()).unwrap_or_else(|e| fail(&e));

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

/// Prints `error` and exits 2, for bad input or config.
fn fail(error: &dyn std::fmt::Display) -> ! {
    eprintln!("error: {error}");
    std::process::exit(2);
}

/// `--config` if given, else `$SIGNAL_CONFIG` or `./signal.yaml`.
fn config_sources(config: Option<&str>) -> Result<Option<Vec<ConfigSource>>, SignalError> {
    match config {
        Some(raw) => Ok(Some(ConfigSource::parse_list(raw)?)),
        None => ConfigSource::discover(),
    }
}

/// A one-shot command loads the config once; there is nothing to reload.
fn one_shot() -> Result<LoadOptions, SignalError> {
    Ok(LoadOptions {
        refresh: None,
        ..LoadOptions::from_env()?
    })
}

fn load_config(config: Option<&str>) -> Result<Config, SignalError> {
    match config_sources(config)? {
        Some(sources) => Config::load(&sources, &one_shot()?),
        None => Ok(Config::default()),
    }
}

fn build_client(config: Option<&str>) -> Result<SignalClient, SignalError> {
    let sources = config_sources(config)?;
    let opts = one_shot()?;
    match sources {
        Some(sources) => SignalClient::from_sources(&sources, &opts),
        None => SignalClient::from_config(&Config::default()),
    }
}
