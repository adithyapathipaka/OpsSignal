//! The `opssignal-azure` add-on: lets the base `opssignal` Python
//! package read `SIGNAL_CONFIG=az://...` from Azure Blob Storage. The base
//! package looks for this module when it builds its client and sends
//! every az:// fetch to [`fetch`]; all the logic is in
//! `opssignal-core`, compiled here with its `azure` feature.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// fetch(url) -> str: the raw text of the config at `url`.
#[pyfunction]
fn fetch(py: Python<'_>, url: String) -> PyResult<String> {
    py.detach(|| opssignal_core::source::fetch_blocking(&url))
        .map_err(PyRuntimeError::new_err)
}

#[pymodule]
fn _azure(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("SCHEME", "az")?;
    m.add_function(wrap_pyfunction!(fetch, m)?)?;
    Ok(())
}
