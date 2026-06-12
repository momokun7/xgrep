//! Glob-based result filtering for the -g/--glob flag (ripgrep-compatible).

use crate::error::{Result, XgrepError};

/// Compiled include/exclude glob filters.
///
/// Globs without a path separator also match against any path depth
/// (`*.rs` behaves like `**/*.rs`), mirroring ripgrep's -g semantics.
pub(crate) struct GlobFilter {
    includes: Vec<Vec<glob::Pattern>>,
    excludes: Vec<Vec<glob::Pattern>>,
}

impl GlobFilter {
    pub(crate) fn new(globs: &[String]) -> Result<Self> {
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for g in globs {
            let (negated, body) = match g.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, g.as_str()),
            };
            let mut patterns = vec![compile(body, g)?];
            if !body.contains('/') {
                patterns.push(compile(&format!("**/{}", body), g)?);
            }
            if negated {
                excludes.push(patterns);
            } else {
                includes.push(patterns);
            }
        }
        Ok(Self { includes, excludes })
    }

    pub(crate) fn matches(&self, path: &str) -> bool {
        let opts = glob::MatchOptions {
            require_literal_separator: true,
            ..Default::default()
        };
        let hit = |group: &[glob::Pattern]| group.iter().any(|p| p.matches_with(path, opts));
        if self.excludes.iter().any(|group| hit(group)) {
            return false;
        }
        self.includes.is_empty() || self.includes.iter().any(|group| hit(group))
    }
}

fn compile(body: &str, original: &str) -> Result<glob::Pattern> {
    glob::Pattern::new(body)
        .map_err(|e| XgrepError::InvalidPattern(format!("invalid glob '{}': {}", original, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_basename_glob_matches_any_depth() {
        let f = GlobFilter::new(&["*.rs".to_string()]).unwrap();
        assert!(f.matches("main.rs"));
        assert!(f.matches("src/deep/lib.rs"));
        assert!(!f.matches("src/lib.py"));
    }

    #[test]
    fn exclude_glob() {
        let f = GlobFilter::new(&["*.rs".to_string(), "!*_test.rs".to_string()]).unwrap();
        assert!(f.matches("src/lib.rs"));
        assert!(!f.matches("src/lib_test.rs"));
    }

    #[test]
    fn path_glob_requires_separator_match() {
        let f = GlobFilter::new(&["src/*.rs".to_string()]).unwrap();
        assert!(f.matches("src/lib.rs"));
        assert!(!f.matches("other/lib.rs"));
        // require_literal_separator: '*' must not cross '/'
        assert!(!f.matches("src/deep/lib.rs"));
    }

    #[test]
    fn exclude_only_means_include_everything_else() {
        let f = GlobFilter::new(&["!vendor/**".to_string()]).unwrap();
        assert!(f.matches("src/lib.rs"));
        assert!(!f.matches("vendor/x/y.js"));
    }

    #[test]
    fn invalid_glob_is_error() {
        assert!(GlobFilter::new(&["[invalid".to_string()]).is_err());
    }
}
