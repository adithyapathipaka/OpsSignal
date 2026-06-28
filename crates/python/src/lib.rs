//! PyO3 bindings. This is intentionally a thin layer: all real logic
//! lives in `opssignal-core`. Per the architecture review, this crate's
//! one additional job is translating Rust `Result::Err` into proper
//! Python exceptions — silent failure here is exactly the kind of bug
//! that erodes trust in a tool whose entire purpose is "tell me when
//! things fail."

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use opssignal_core::signal::{Severity, SignalInput};

fn severity_from_str(s: &str) -> Severity {
    match s.to_lowercase().as_str() {
        "success" => Severity::Success,
        "warning" => Severity::Warning,
        "error" => Severity::Error,
        "critical" => Severity::Critical,
        _ => Severity::Info,
    }
}

/// notify(source, event_type, title, severity="info", message=None,
///         environment=None, metadata=None)
///
/// v0.1 scope note: this currently validates and constructs a Signal
/// and returns immediately (fire-and-forget semantics matching
/// notify_async in the core). Wiring to a configured SignalClient
/// (reading routing/providers from signal.yaml) is the next
/// implementation step — see crates/core/src/lib.rs SignalClient.
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
    let input = SignalInput {
        source,
        event_type,
        title,
        severity: severity_from_str(severity),
        message,
        environment,
        ..Default::default()
    };

    opssignal_core::signal::validate(&input)
        .map_err(|e| PyValueError::new_err(format!("invalid signal: {e}")))?;

    // TODO(v0.1): hold a process-global SignalClient (lazily built from
    // signal.yaml on first call) and call client.notify_async(input)
    // here instead of just validating and discarding.
    eprintln!("[opssignal] notify() validated input but is not yet wired to a SignalClient");

    Ok(())
}

#[pymodule]
fn _native(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(notify, m)?)?;
    Ok(())
}
