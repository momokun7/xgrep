use pyo3::prelude::*;

#[pymodule]
fn _xg(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    Ok(())
}
