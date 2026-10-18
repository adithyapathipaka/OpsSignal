//! The `opssignal-aws` add-on: lets the base `opssignal` Python
//! package read `SIGNAL_CONFIG=s3://...` from AWS S3. The base
//! package looks for this module when it builds its client and sends
//! every s3:// fetch to [`fetch`]; all the logic is in
//! `opssignal-core`, compiled here with its `s3` feature.

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
    m.add("SCHEME", "s3")?;
    m.add_function(wrap_pyfunction!(fetch, m)?)?;
    Ok(())
}
