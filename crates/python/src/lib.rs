//! PyO3 bindings. This is intentionally a thin layer: all real logic
//! lives in `opssignal-core`. Per the architecture review, this crate's
//! one additional job is translating Rust `Result::Err` into proper
//! Python exceptions — silent failure here is exactly the kind of bug
//! that erodes trust in a tool whose entire purpose is "tell me when
//! things fail."

use opssignal_core::delivery::SendReport;
use opssignal_core::providers::external::{register_cloud_sender, ExternalSender};
use opssignal_core::signal::{Action, Severity, SignalInput};
use opssignal_core::source::{register_cloud_fetcher, ExternalFetcher};
use opssignal_core::SignalClient;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
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
///         environment=None, *, service=None, component=None, team=None,
///         run_id=None, correlation_id=None, trace_id=None, log_url=None,
///         metadata=None, dedup_key=None, action="trigger")
///
/// Validates the signal, then hands it to the process-global
/// SignalClient with fire-and-forget semantics (`notify_async`).
/// `metadata` is any JSON-serialisable object, usually a dict. Raises
/// ValueError for invalid input and RuntimeError if signal.yaml cannot
/// be loaded.
///
/// `action` is "trigger", "acknowledge" or "resolve"; the last two act
/// on the alert with the same dedup key and may leave source,
/// event_type and title empty when dedup_key is given. An unknown
/// action raises ValueError: guessing could page someone for an alert
/// meant to be closed.
#[pyfunction]
#[pyo3(signature = (
    source, event_type, title, severity="info", message=None, environment=None, *,
    service=None, component=None, team=None, run_id=None, correlation_id=None,
    trace_id=None, log_url=None, metadata=None, dedup_key=None, action="trigger",
))]
#[allow(clippy::too_many_arguments)]
fn notify(
    py: Python<'_>,
    source: String,
    event_type: String,
    title: String,
    severity: &str,
    message: Option<String>,
    environment: Option<String>,
    service: Option<String>,
    component: Option<String>,
    team: Option<String>,
    run_id: Option<String>,
    correlation_id: Option<String>,
    trace_id: Option<String>,
    log_url: Option<String>,
    metadata: Option<&Bound<'_, PyAny>>,
    dedup_key: Option<String>,
    action: &str,
) -> PyResult<()> {
    let input = signal_input(
        py,
        source,
        event_type,
        title,
        severity,
        message,
        environment,
        service,
        component,
        team,
        run_id,
        correlation_id,
        trace_id,
        log_url,
        metadata,
        dedup_key,
        action,
    )?;
    client(py)?.notify_async(input);
    Ok(())
}

/// send(source, event_type, title, severity="info", message=None,
///       environment=None, *, service=None, component=None, team=None,
///       run_id=None, correlation_id=None, trace_id=None, log_url=None,
///       metadata=None, dedup_key=None, action="trigger", timeout=30.0)
///       -> DeliveryResult
///
/// notify() that waits for delivery, up to `timeout` seconds, and
/// returns how it went. A failed or partial delivery is reported in the
/// result, not raised: check `result.status` or `result.delivered`.
/// Raises ValueError for invalid input or timeout, and RuntimeError if
/// signal.yaml cannot be loaded or the signal cannot be recorded.
/// `action` is as for notify(), so a resolve can wait for its result.
#[pyfunction]
#[pyo3(signature = (
    source, event_type, title, severity="info", message=None, environment=None, *,
    service=None, component=None, team=None, run_id=None, correlation_id=None,
    trace_id=None, log_url=None, metadata=None, dedup_key=None, action="trigger",
    timeout=30.0,
))]
#[allow(clippy::too_many_arguments)]
fn send(
    py: Python<'_>,
    source: String,
    event_type: String,
    title: String,
    severity: &str,
    message: Option<String>,
    environment: Option<String>,
    service: Option<String>,
    component: Option<String>,
    team: Option<String>,
    run_id: Option<String>,
    correlation_id: Option<String>,
    trace_id: Option<String>,
    log_url: Option<String>,
    metadata: Option<&Bound<'_, PyAny>>,
    dedup_key: Option<String>,
    action: &str,
    timeout: f64,
) -> PyResult<DeliveryResult> {
    let timeout = Duration::try_from_secs_f64(timeout)
        .map_err(|e| PyValueError::new_err(format!("invalid timeout: {e}")))?;
    let input = signal_input(
        py,
        source,
        event_type,
        title,
        severity,
        message,
        environment,
        service,
        component,
        team,
        run_id,
        correlation_id,
        trace_id,
        log_url,
        metadata,
        dedup_key,
        action,
    )?;
    let client = client(py)?;
    // Detached: a cloud add-on provider needs the GIL from a delivery
    // thread while this one waits.
    let report = py
        .detach(|| client.send(input, timeout))
        .map_err(|e| PyRuntimeError::new_err(format!("signal not sent: {e}")))?;
    Ok(DeliveryResult::from(report))
}

/// Builds and validates the input shared by notify() and send().
#[allow(clippy::too_many_arguments)]
fn signal_input(
    py: Python<'_>,
    source: String,
    event_type: String,
    title: String,
    severity: &str,
    message: Option<String>,
    environment: Option<String>,
    service: Option<String>,
    component: Option<String>,
    team: Option<String>,
    run_id: Option<String>,
    correlation_id: Option<String>,
    trace_id: Option<String>,
    log_url: Option<String>,
    metadata: Option<&Bound<'_, PyAny>>,
    dedup_key: Option<String>,
    action: &str,
) -> PyResult<SignalInput> {
    let action = action
        .parse::<Action>()
        .map_err(|e| PyValueError::new_err(format!("invalid signal: {e}")))?;
    // Unknown severities fall back to Info rather than raising — the
    // Python binding prioritises leniency over strictness here so that
    // a misconfigured severity string never silences an alert.
    let severity = severity.parse::<Severity>().unwrap_or_default();

    let metadata = match metadata {
        Some(obj) if !obj.is_none() => Some(to_json(py, obj)?),
        _ => None,
    };

    let input = SignalInput {
        source,
        event_type,
        title,
        severity,
        message,
        environment,
        service,
        component,
        team,
        run_id,
        correlation_id,
        trace_id,
        log_url,
        metadata,
        dedup_key,
        action,
    };

    opssignal_core::signal::validate(&input)
        .map_err(|e| PyValueError::new_err(format!("invalid signal: {e}")))?;
    Ok(input)
}

/// What send() returns. `status` is "delivered", "partial", "failed",
/// "skipped" (every matching route held it back by dedup or
/// rate_limit; `suppressed` says why) or "grouped" (held for a digest;
/// `grouped` says which routes).
#[pyclass(frozen, module = "opssignal", get_all)]
struct DeliveryResult {
    /// The id the signal is stored under in the SQLite history.
    signal_id: String,
    /// The incident key, given or computed.
    dedup_key: Option<String>,
    status: String,
    /// The providers that delivered, comma separated, or None.
    provider: Option<String>,
    /// Why delivery failed, per provider when there were several.
    error: Option<String>,
    attempts: u32,
    /// One ProviderResult per provider tried, in plan order.
    deliveries: Vec<ProviderResult>,
    /// Routes that held the signal back, with the reason.
    suppressed: Vec<String>,
    /// Routes that held the signal for a digest.
    grouped: Vec<String>,
}

#[pymethods]
impl DeliveryResult {
    /// True only if every provider the signal was routed to got it.
    #[getter]
    fn delivered(&self) -> bool {
        self.status == "delivered"
    }

    /// The result as plain dicts and lists, for logging or JSON.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("signal_id", &self.signal_id)?;
        d.set_item("dedup_key", &self.dedup_key)?;
        d.set_item("status", &self.status)?;
        d.set_item("provider", &self.provider)?;
        d.set_item("error", &self.error)?;
        d.set_item("attempts", self.attempts)?;
        let deliveries = self
            .deliveries
            .iter()
            .map(|p| p.to_dict(py))
            .collect::<PyResult<Vec<_>>>()?;
        d.set_item("deliveries", deliveries)?;
        d.set_item("suppressed", &self.suppressed)?;
        d.set_item("grouped", &self.grouped)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "DeliveryResult(status={:?}, provider={:?}, signal_id={:?})",
            self.status, self.provider, self.signal_id
        )
    }
}

/// How delivery to one provider went: "delivered" or "failed"
/// ("pending" if send() timed out while it was still trying).
#[pyclass(frozen, module = "opssignal", get_all, skip_from_py_object)]
#[derive(Clone)]
struct ProviderResult {
    provider: String,
    status: String,
    error: Option<String>,
    attempts: u32,
}

#[pymethods]
impl ProviderResult {
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("provider", &self.provider)?;
        d.set_item("status", &self.status)?;
        d.set_item("error", &self.error)?;
        d.set_item("attempts", self.attempts)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "ProviderResult(provider={:?}, status={:?}, attempts={})",
            self.provider, self.status, self.attempts
        )
    }
}

impl From<SendReport> for DeliveryResult {
    fn from(report: SendReport) -> Self {
        let r = report.result;
        DeliveryResult {
            signal_id: report.signal_id.to_string(),
            dedup_key: report.dedup_key,
            status: r.status.to_string(),
            provider: r.provider,
            error: r.error,
            attempts: r.attempts,
            deliveries: r
                .deliveries
                .into_iter()
                .map(|o| ProviderResult {
                    provider: o.provider,
                    status: o.status.to_string(),
                    error: o.error,
                    attempts: o.attempts,
                })
                .collect(),
            suppressed: r.suppressed,
            grouped: r.grouped,
        }
    }
}

/// Converts a Python object to JSON through the standard `json` module,
/// with `default=str` so dates and other common values don't make an
/// alert fail.
fn to_json(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    let json = py.import("json")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("default", py.import("builtins")?.getattr("str")?)?;
    let text: String = json
        .call_method("dumps", (obj,), Some(&kwargs))
        .map_err(|e| PyValueError::new_err(format!("metadata is not JSON-serialisable: {e}")))?
        .extract()?;
    serde_json::from_str(&text)
        .map_err(|e| PyValueError::new_err(format!("metadata is not valid JSON: {e}")))
}

/// flush(timeout=5.0) -> bool
///
/// Sends the digests of any open groups (routes with `group_by` or
/// `group_wait`), then waits up to `timeout` seconds for signals sent
/// with notify() to finish delivering. Returns False if some were still in flight; they
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
    m.add_function(wrap_pyfunction!(send, m)?)?;
    m.add_function(wrap_pyfunction!(flush, m)?)?;
    m.add_class::<DeliveryResult>()?;
    m.add_class::<ProviderResult>()?;
    Ok(())
}
