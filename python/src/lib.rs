use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[pyclass]
pub struct SearchResult {
    #[pyo3(get)]
    pub file: String,
    #[pyo3(get)]
    pub line_number: u32,
    #[pyo3(get)]
    pub line: String,
}

#[pymethods]
impl SearchResult {
    fn __repr__(&self) -> String {
        format!(
            "SearchResult(file={:?}, line_number={}, line={:?})",
            self.file, self.line_number, self.line
        )
    }
}

#[pyclass]
pub struct IndexStatus {
    #[pyo3(get)]
    pub state: String,
    #[pyo3(get)]
    pub changed_files: Option<u32>,
    #[pyo3(get)]
    pub indexed_files: u32,
    #[pyo3(get)]
    pub index_size_bytes: i64,
    #[pyo3(get)]
    pub index_path: String,
}

#[pymethods]
impl IndexStatus {
    fn __repr__(&self) -> String {
        format!(
            "IndexStatus(state={:?}, indexed_files={})",
            self.state, self.indexed_files
        )
    }
}

#[pyclass]
pub struct Xgrep {
    inner: xgrep_search::Xgrep,
}

#[pymethods]
impl Xgrep {
    #[staticmethod]
    pub fn open(root: &str) -> PyResult<Xgrep> {
        let inner = xgrep_search::Xgrep::open(root)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Xgrep { inner })
    }

    #[staticmethod]
    pub fn open_local(root: &str) -> PyResult<Xgrep> {
        let inner = xgrep_search::Xgrep::open_local(root)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Xgrep { inner })
    }

    pub fn build_index(&self) -> PyResult<()> {
        self.inner
            .build_index()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    #[pyo3(signature = (
        pattern,
        *,
        case_insensitive=false,
        regex=false,
        file_type=None,
        max_count=None,
        changed_only=false,
        since=None,
        path_pattern=None,
        fresh=false,
        word=false,
        globs=None
    ))]
    pub fn search(
        &self,
        pattern: &str,
        case_insensitive: bool,
        regex: bool,
        file_type: Option<String>,
        max_count: Option<usize>,
        changed_only: bool,
        since: Option<String>,
        path_pattern: Option<String>,
        fresh: bool,
        word: bool,
        globs: Option<Vec<String>>,
    ) -> PyResult<Vec<SearchResult>> {
        let opts = xgrep_search::SearchOptions {
            case_insensitive,
            regex,
            file_type,
            max_count,
            changed_only,
            since,
            path_pattern,
            fresh,
            word,
            globs: globs.unwrap_or_default(),
        };
        let results = self
            .inner
            .search(pattern, &opts)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(results
            .into_iter()
            .map(|r| SearchResult {
                file: r.file.to_string(),
                line_number: r.line_number as u32,
                line: r.line,
            })
            .collect())
    }

    pub fn index_status(&self) -> PyResult<IndexStatus> {
        let info = self
            .inner
            .index_status()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let (state, changed_files) = match info.state {
            xgrep_search::IndexState::Fresh => ("fresh".to_string(), None),
            xgrep_search::IndexState::Stale { changed_files } => {
                ("stale".to_string(), Some(changed_files as u32))
            }
            xgrep_search::IndexState::Missing => ("missing".to_string(), None),
        };
        Ok(IndexStatus {
            state,
            changed_files,
            indexed_files: info.indexed_files as u32,
            index_size_bytes: info.index_size_bytes as i64,
            index_path: info.index_path.to_string_lossy().to_string(),
        })
    }

    #[getter]
    pub fn root(&self) -> String {
        self.inner.root().to_string_lossy().to_string()
    }

    #[getter]
    pub fn index_path(&self) -> String {
        self.inner.index_path().to_string_lossy().to_string()
    }
}

#[pymodule]
fn _xg(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Xgrep>()?;
    m.add_class::<SearchResult>()?;
    m.add_class::<IndexStatus>()?;
    Ok(())
}
