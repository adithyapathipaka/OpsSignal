//! Providers that a cloud add-on delivers for a build without that
//! cloud compiled in, the way [`crate::source::ExternalFetcher`] serves
//! cloud config sources.
//!
//! The base Python wheel has no cloud code. When a config names a
//! cloud-only transport (email over Amazon SES, say), the provider hands
//! the provider config and the signal, as JSON, to the sender the
//! `opssignal[aws]` add-on registered. The add-on runs the same core
//! code with its cloud feature on, through [`send_blocking`].

use crate::signal::{Signal, SignalError};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// Guards against a sender that never returns; the sender applies its
/// own, shorter request timeouts.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Delivers one signal through a cloud-only transport. `provider` is
/// the provider's name (`email`), `config_json` its config and
/// `signal_json` the signal, both as JSON.
pub trait ExternalSender: Send + Sync {
    fn send(&self, provider: &str, config_json: &str, signal_json: &str) -> Result<(), String>;
}

type SenderRegistry = RwLock<HashMap<String, Arc<dyn ExternalSender>>>;

fn senders() -> &'static SenderRegistry {
    static SENDERS: OnceLock<SenderRegistry> = OnceLock::new();
    SENDERS.get_or_init(Default::default)
}

/// Registers the sender for every cloud-only transport of `cloud`
/// (`aws`, `gcp` or `azure`). This is what an add-on package does.
pub fn register_cloud_sender(cloud: &str, sender: Arc<dyn ExternalSender>) {
    senders()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cloud.to_string(), sender);
}

fn registered_sender(cloud: &str) -> Option<Arc<dyn ExternalSender>> {
    senders()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(cloud)
        .cloned()
}

/// Whether this build can deliver through `cloud`'s transports, natively
/// or through a registered add-on.
pub fn cloud_supported(cloud: &str) -> bool {
    let built_in = (cloud == "aws" && cfg!(feature = "aws"))
        || (cloud == "gcp" && cfg!(feature = "gcp"))
        || (cloud == "azure" && cfg!(feature = "azure"));
    built_in || registered_sender(cloud).is_some()
}

/// The error for a config that needs a cloud this build lacks.
pub(crate) fn missing_support(what: &str, cloud: &str) -> String {
    format!(
        "{what} needs the {cloud} add-on: pip install 'opssignal[{cloud}]' for Python, \
         or a CLI or C library built with `--features {cloud}`"
    )
}

// Only a build without the cloud compiled in hands off to an add-on.
#[cfg_attr(feature = "aws", allow(dead_code))]
/// Hands a delivery to `cloud`'s registered sender.
pub(crate) async fn send_external(
    cloud: &str,
    provider: &str,
    config_json: String,
    signal: &Signal,
) -> Result<(), SignalError> {
    let sender = registered_sender(cloud)
        .ok_or_else(|| SignalError::Delivery(missing_support(provider, cloud)))?;
    let signal_json =
        serde_json::to_string(signal).map_err(|e| SignalError::Delivery(e.to_string()))?;
    let provider = provider.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A plain thread rather than spawn_blocking: the sender may start
    // its own runtime, which tokio forbids on its blocking pool.
    std::thread::Builder::new()
        .name("opssignal-send".into())
        .spawn(move || {
            let _ = tx.send(sender.send(&provider, &config_json, &signal_json));
        })
        .map_err(|e| SignalError::Delivery(format!("cannot start send thread: {e}")))?;
    match tokio::time::timeout(SEND_TIMEOUT, rx).await {
        Ok(Ok(result)) => result.map_err(SignalError::Delivery),
        Ok(Err(_)) => Err(SignalError::Delivery(
            "the add-on sender failed without an answer".into(),
        )),
        Err(_) => Err(SignalError::Delivery(format!(
            "timed out after {SEND_TIMEOUT:?}"
        ))),
    }
}

/// Delivers a signal for an add-on, on a throwaway runtime, with errors
/// in the form an [`ExternalSender`] returns them. This is what add-on
/// packages call to serve a send.
pub fn send_blocking(provider: &str, config_json: &str, signal_json: &str) -> Result<(), String> {
    use crate::delivery::Provider;

    let signal: Signal =
        serde_json::from_str(signal_json).map_err(|e| format!("invalid signal: {e}"))?;
    let provider: Box<dyn Provider> = match provider {
        "email" => {
            let config = serde_json::from_str(config_json)
                .map_err(|e| format!("invalid email config: {e}"))?;
            Box::new(super::email::EmailProvider::new(config))
        }
        other => return Err(format!("unknown provider {other:?}")),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start runtime: {e}"))?;
    runtime
        .block_on(provider.send(&signal))
        .map_err(|e| match e {
            SignalError::Delivery(msg) => msg,
            other => other.to_string(),
        })
}
