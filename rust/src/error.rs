/// Error types for xgrep.
///
/// Library consumers can match on error variants to distinguish
/// between different failure modes (e.g., index corruption vs I/O errors).
#[derive(thiserror::Error, Debug)]
pub enum XgrepError {
    /// The directory is not inside a git repository.
    #[error("not a git repository")]
    NotGitRepo,

    /// Invalid search pattern (regex syntax error, invalid glob, etc.)
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),

    /// Index-related error (corrupt, version mismatch, truncated, build failure).
    #[error("index error: {0}")]
    IndexError(String),

    /// Failed to acquire or create a lock file.
    #[error("lock error: {0}")]
    LockError(String),

    /// Underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias used throughout the library.
pub type Result<T> = std::result::Result<T, XgrepError>;
