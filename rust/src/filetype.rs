/// Map type name to file extensions
pub fn extensions_for_type(type_name: &str) -> Option<Vec<&'static str>> {
    let exts = match type_name {
        "rust" | "rs" => vec!["rs"],
        "python" | "py" => vec!["py", "pyi"],
        "javascript" | "js" => vec!["js", "mjs", "cjs"],
        "typescript" | "ts" => vec!["ts", "tsx", "mts"],
        "go" => vec!["go"],
        "ruby" | "rb" => vec!["rb"],
        "java" => vec!["java"],
        "c" => vec!["c", "h"],
        "cpp" | "cc" => vec!["cpp", "cc", "cxx", "hpp", "hxx", "h"],
        "shell" | "sh" => vec!["sh", "bash", "zsh"],
        "json" => vec!["json"],
        "yaml" | "yml" => vec!["yaml", "yml"],
        "markdown" | "md" => vec!["md", "markdown"],
        "html" => vec!["html", "htm"],
        "css" => vec!["css"],
        "sql" => vec!["sql"],
        "toml" => vec!["toml"],
        "xml" => vec!["xml"],
        _ => return None,
    };
    Some(exts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extensions_for_type() {
        assert_eq!(extensions_for_type("rs"), Some(vec!["rs"]));
        assert_eq!(extensions_for_type("rust"), Some(vec!["rs"]));
        assert_eq!(extensions_for_type("py"), Some(vec!["py", "pyi"]));
        assert_eq!(extensions_for_type("unknown"), None);
    }

    #[test]
    fn test_extensions_all_types() {
        // Verify all documented types return Some
        let types = [
            "rust",
            "rs",
            "python",
            "py",
            "javascript",
            "js",
            "typescript",
            "ts",
            "go",
            "ruby",
            "rb",
            "java",
            "c",
            "cpp",
            "cc",
            "shell",
            "sh",
            "json",
            "yaml",
            "yml",
            "markdown",
            "md",
            "html",
            "css",
            "sql",
            "toml",
            "xml",
        ];
        for t in types {
            assert!(
                extensions_for_type(t).is_some(),
                "type '{}' should be supported",
                t
            );
        }
    }

    #[test]
    fn test_extensions_none_for_unknown() {
        assert_eq!(extensions_for_type("fortran"), None);
        assert_eq!(extensions_for_type(""), None);
        assert_eq!(extensions_for_type("RUST"), None); // case sensitive
    }
}
