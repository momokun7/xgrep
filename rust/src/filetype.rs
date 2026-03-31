/// Map type name to file extensions
pub fn extensions_for_type(type_name: &str) -> Option<Vec<&'static str>> {
    let lower = type_name.to_ascii_lowercase();
    let exts = match lower.as_str() {
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
        "kotlin" | "kt" => vec!["kt", "kts"],
        "swift" => vec!["swift"],
        "dart" => vec!["dart"],
        "gradle" => vec!["gradle"],
        "proto" | "protobuf" => vec!["proto"],
        "zig" => vec!["zig"],
        "elixir" | "ex" => vec!["ex", "exs"],
        "php" => vec!["php"],
        "scala" => vec!["scala", "sc"],
        "r" => vec!["r", "R"],
        "lua" => vec!["lua"],
        "haskell" | "hs" => vec!["hs"],
        "terraform" | "tf" => vec!["tf", "tfvars"],
        "jsx" => vec!["jsx"],
        "vue" => vec!["vue"],
        "svelte" => vec!["svelte"],
        _ => return None,
    };
    Some(exts)
}

/// Return all supported type names and their extensions, sorted alphabetically by type name.
pub fn list_all_types() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut types: Vec<(&str, Vec<&str>)> = vec![
        ("c", vec!["c", "h"]),
        ("cpp", vec!["cpp", "cc", "cxx", "hpp", "hxx", "h"]),
        ("css", vec!["css"]),
        ("dart", vec!["dart"]),
        ("elixir", vec!["ex", "exs"]),
        ("go", vec!["go"]),
        ("gradle", vec!["gradle"]),
        ("haskell", vec!["hs"]),
        ("html", vec!["html", "htm"]),
        ("java", vec!["java"]),
        ("javascript", vec!["js", "mjs", "cjs"]),
        ("json", vec!["json"]),
        ("jsx", vec!["jsx"]),
        ("kotlin", vec!["kt", "kts"]),
        ("lua", vec!["lua"]),
        ("markdown", vec!["md", "markdown"]),
        ("php", vec!["php"]),
        ("proto", vec!["proto"]),
        ("python", vec!["py", "pyi"]),
        ("r", vec!["r", "R"]),
        ("ruby", vec!["rb"]),
        ("rust", vec!["rs"]),
        ("scala", vec!["scala", "sc"]),
        ("shell", vec!["sh", "bash", "zsh"]),
        ("sql", vec!["sql"]),
        ("svelte", vec!["svelte"]),
        ("swift", vec!["swift"]),
        ("terraform", vec!["tf", "tfvars"]),
        ("toml", vec!["toml"]),
        ("typescript", vec!["ts", "tsx", "mts"]),
        ("vue", vec!["vue"]),
        ("xml", vec!["xml"]),
        ("yaml", vec!["yaml", "yml"]),
        ("zig", vec!["zig"]),
    ];
    types.sort_by_key(|(name, _)| *name);
    types
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
            "kotlin",
            "kt",
            "swift",
            "dart",
            "gradle",
            "proto",
            "protobuf",
            "zig",
            "elixir",
            "ex",
            "php",
            "scala",
            "r",
            "lua",
            "haskell",
            "hs",
            "terraform",
            "tf",
            "jsx",
            "vue",
            "svelte",
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
        assert_eq!(extensions_for_type("RUST"), Some(vec!["rs"])); // case insensitive
    }

    #[test]
    fn test_extensions_case_insensitive() {
        assert_eq!(extensions_for_type("RUST"), Some(vec!["rs"]));
        assert_eq!(extensions_for_type("Py"), Some(vec!["py", "pyi"]));
        assert_eq!(
            extensions_for_type("JavaScript"),
            Some(vec!["js", "mjs", "cjs"])
        );
        assert_eq!(extensions_for_type("GO"), Some(vec!["go"]));
    }

    #[test]
    fn test_list_all_types_consistent_with_extensions_for_type() {
        // Ensure every type in list_all_types is also recognized by extensions_for_type
        // and returns the same extensions.
        for (name, exts) in list_all_types() {
            let result = extensions_for_type(name);
            assert_eq!(
                result,
                Some(exts.clone()),
                "list_all_types entry '{}' does not match extensions_for_type",
                name
            );
        }
    }
}
