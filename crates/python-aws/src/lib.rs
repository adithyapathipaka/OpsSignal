//! The `opssignal-aws` add-on: lets the base `opssignal` Python
//! package read configs from AWS S3 and AppConfig (s3:// and appconfig://). The base package
//! looks for this module when it builds its client and sends every
//! fetch for those schemes to [`fetch`]; all the logic is in
//! `opssignal-core`, compiled here with its `aws` feature.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// fetch(url) -> str: the raw text of the config at `url`.
#[pyfunction]
fn fetch(py: Python<'_>, url: String) -> PyResult<String> {
    py.detach(|| opssignal_core::source::fetch_blocking(&url))
        .map_err(PyRuntimeError::new_err)
}

#[pymodule]
fn _aws(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("CLOUD", "aws")?;
    m.add_function(wrap_pyfunction!(fetch, m)?)?;
    Ok(())
}
