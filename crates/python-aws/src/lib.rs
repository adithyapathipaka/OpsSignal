//! The `opssignal-aws` add-on: lets the base `opssignal` Python
//! package read configs from AWS (s3://, appconfig://, dynamodb://) and
//! Secrets Manager placeholders (`${aws-sm:...}`), and send email
//! through Amazon SES. The base package
//! looks for this module when it builds its client and sends every
//! fetch for those schemes to [`fetch`] and every SES send to [`send`];
//! all the logic is in
//! `opssignal-core`, compiled here with its `aws` feature.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// fetch(url) -> str: the raw text of the config at `url`.
#[pyfunction]
fn fetch(py: Python<'_>, url: String) -> PyResult<String> {
    py.detach(|| opssignal_core::source::fetch_blocking(&url))
        .map_err(PyRuntimeError::new_err)
}

/// send(provider, config_json, signal_json): delivers one signal
/// through an AWS-only transport (email over SES) for the base package.
#[pyfunction]
fn send(
    py: Python<'_>,
    provider: String,
    config_json: String,
    signal_json: String,
) -> PyResult<()> {
    py.detach(|| {
        opssignal_core::providers::external::send_blocking(&provider, &config_json, &signal_json)
    })
    .map_err(PyRuntimeError::new_err)
}

#[pymodule]
fn _aws(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("CLOUD", "aws")?;
    m.add_function(wrap_pyfunction!(fetch, m)?)?;
    m.add_function(wrap_pyfunction!(send, m)?)?;
    Ok(())
}
