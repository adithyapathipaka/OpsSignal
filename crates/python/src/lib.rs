//! PyO3 bindings. This is intentionally a thin layer: all real logic
//! lives in `opssignal-core`. Per the architecture review, this crate's
//! one additional job is translating Rust `Result::Err` into proper
//! Python exceptions — silent failure here is exactly the kind of bug
//! that erodes trust in a tool whose entire purpose is "tell me when
//! things fail."

use opssignal_core::providers::external::{register_cloud_sender, ExternalSender};
use opssignal_core::signal::{Severity, SignalInput};
use opssignal_core::source::{register_cloud_fetcher, ExternalFetcher};
use opssignal_core::SignalClient;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Process-global client, built from signal.yaml on the first notify()
/// call. A config error is cached too, so every later call raises the
/// same error instead of re-reading a broken file on each alert.
static CLIENT: OnceLock<Result<SignalClient, String>> = OnceLock::new();

/// Cloud add-ons installed as pip extras. Each serves every URL scheme
/// of its cloud (see `opssignal_core::source::CLOUD_SCHEMES`).
const ADDONS: [(&str, &str); 3] = [
    ("aws", "opssignal_aws"),
    ("gcp", "opssignal_gcp"),
    ("azure", "opssignal_azure"),
];

/// Hands a core fetch request to an add-on's `fetch(url) -> str`.
struct AddonFetcher(Py<PyAny>);

impl ExternalFetcher for AddonFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        // try_attach: a reload can fire while the interpreter exits.
        Python::try_attach(|py| {
            self.0
                .call1(py, (url,))
                .and_then(|text| text.extract::<String>(py))
                .map_err(|e| e.to_string())
        })
        .unwrap_or_else(|| Err("the Python interpreter is shutting down".to_string()))
    }
}

/// Hands a core send to an add-on's `send(provider, config, signal)`.
struct AddonSender(Py<PyAny>);

impl ExternalSender for AddonSender {
    fn send(&self, provider: &str, config_json: &str, signal_json: &str) -> Result<(), String> {
        Python::try_attach(|py| {
            self.0
                .call1(py, (provider, config_json, signal_json))
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .unwrap_or_else(|| Err("the Python interpreter is shutting down".to_string()))
    }
}

fn register_addons(py: Python<'_>) {
    for (cloud, module) in ADDONS {
        let Ok(module) = py.import(module) else {
            continue;
        };
        if let Ok(fetch) = module.getattr("fetch") {
            register_cloud_fetcher(cloud, Arc::new(AddonFetcher(fetch.unbind())));
        }
        if let Ok(send) = module.getattr("send") {
            register_cloud_sender(cloud, Arc::new(AddonSender(send.unbind())));
        }
    }
}

fn client(py: Python<'_>) -> PyResult<&'static SignalClient> {
    if CLIENT.get().is_none() {
        register_addons(py);
    }
    // Detached: loading the config may call an add-on, which needs the
    // GIL from another thread, and a second Python thread waiting here
    // must not hold it either.
    py.detach(|| CLIENT.get_or_init(|| SignalClient::from_env().map_err(|e| e.to_string())))
        .as_ref()
        .map_err(|e| PyRuntimeError::new_err(format!("opssignal is not configured: {e}")))
}

/// notify(source, event_type, title, severity="info", message=None,
///         environment=None, metadata=None)
///
/// Validates the signal, then hands it to the process-global
/// SignalClient with fire-and-forget semantics (`notify_async`).
/// Raises ValueError for invalid input and RuntimeError if signal.yaml
/// cannot be loaded.
#[pyfunction]
#[pyo3(signature = (source, event_type, title, severity="info", message=None, environment=None))]
fn notify(
    py: Python<'_>,
    source: String,
    event_type: String,
    title: String,
    severity: &str,
    message: Option<String>,
    environment: Option<String>,
) -> PyResult<()> {
    // Unknown severities fall back to Info rather than raising — the
    // Python binding prioritises leniency over strictness here so that
    // a misconfigured severity string never silences an alert.
    let severity = severity.parse::<Severity>().unwrap_or_default();

    let input = SignalInput {
        source,
        event_type,
        title,
        severity,
        message,
        environment,
        ..Default::default()
    };

    opssignal_core::signal::validate(&input)
        .map_err(|e| PyValueError::new_err(format!("invalid signal: {e}")))?;

    client(py)?.notify_async(input);
    Ok(())
}

/// flush(timeout=5.0) -> bool
///
/// Waits up to `timeout` seconds for signals sent with notify() to
/// finish delivering. Returns False if some were still in flight; they
/// stay recorded as pending. Registered with atexit on import, so a
/// script that ends right after notify() still delivers.
#[pyfunction]
#[pyo3(signature = (timeout=5.0))]
fn flush(py: Python<'_>, timeout: f64) -> PyResult<bool> {
    let timeout = Duration::try_from_secs_f64(timeout)
        .map_err(|e| PyValueError::new_err(format!("invalid timeout: {e}")))?;
    let Some(Ok(client)) = CLIENT.get() else {
        return Ok(true);
    };
    // Detached: delivery tasks may need the GIL (a cloud add-on).
    Ok(py.detach(|| client.flush(timeout)))
}

#[pymodule]
fn _native(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(notify, m)?)?;
    m.add_function(wrap_pyfunction!(flush, m)?)?;
    Ok(())
}
