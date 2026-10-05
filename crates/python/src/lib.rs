//! PyO3 bindings. This is intentionally a thin layer: all real logic
//! lives in `opssignal-core`. Per the architecture review, this crate's
//! one additional job is translating Rust `Result::Err` into proper
//! Python exceptions — silent failure here is exactly the kind of bug
//! that erodes trust in a tool whose entire purpose is "tell me when
//! things fail."

use opssignal_core::signal::{Severity, SignalInput};
use opssignal_core::SignalClient;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::sync::OnceLock;

/// Process-global client, built from signal.yaml on the first notify()
/// call. A config error is cached too, so every later call raises the
/// same error instead of re-reading a broken file on each alert.
static CLIENT: OnceLock<Result<SignalClient, String>> = OnceLock::new();

fn client() -> PyResult<&'static SignalClient> {
    CLIENT
        .get_or_init(|| SignalClient::from_env().map_err(|e| e.to_string()))
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

    client()?.notify_async(input);
    Ok(())
}

#[pymodule]
fn _native(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(notify, m)?)?;
    Ok(())
}
