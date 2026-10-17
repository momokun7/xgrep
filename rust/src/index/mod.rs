pub mod builder;
pub(crate) mod cache;
pub mod format;
pub mod reader;
pub mod updater;

use std::path::{Component, Path};

/// A root-relative path as stored in the index and shown in results: its
/// components joined with `/` on every platform, so globs, path filters,
/// git paths and index lookups agree on Windows too.
pub(crate) fn rel_key(rel: &Path) -> String {
    let mut out = String::new();
    for c in rel.components() {
        let part = match c {
            Component::Normal(s) => s.to_string_lossy(),
            Component::ParentDir => "..".into(),
            _ => continue,
        };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::rel_key;
    use std::path::{Path, PathBuf};

    #[test]
    fn rel_key_joins_components_with_slash() {
        let p: PathBuf = ["src", "deep", "b.rs"].iter().collect();
        assert_eq!(rel_key(&p), "src/deep/b.rs");
        assert_eq!(rel_key(Path::new("./a/../b")), "a/../b");
        assert_eq!(rel_key(Path::new("")), "");
    }
}
