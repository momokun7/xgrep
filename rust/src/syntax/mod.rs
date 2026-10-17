//! Syntax-aware features: symbol tags per file (tree-sitter), outlines,
//! enclosing-block lookup, references and the repo map.
//!
//! Layout:
//! - this module: language detection, the [`FileTags`] model and helpers that
//!   work on it (enclosing symbol, outline rendering). Always compiled.
//! - [`parse`]: tree-sitter parsing + tags queries (`syntax` cargo feature).
//!   Without the feature only Markdown headings are recognised and every
//!   caller falls back to the regex heuristics.
//! - [`cache`]: the `<index>.tags` sidecar cache keyed by path + mtime + size,
//!   filled lazily and in parallel. The main index format is not touched.
//! - [`repo_map`]: defs/refs graph + PageRank (Aider-style map).
//! - [`tools`]: MCP handlers (`outline`, `find_references`, `repo_map`) and
//!   block expansion for `search`.

pub mod cache;
#[cfg(feature = "syntax")]
pub mod parse;
pub mod repo_map;
pub mod tools;

use std::path::Path;

/// Files larger than this are not parsed (generated parsers, bundles, data).
pub const MAX_PARSE_BYTES: u64 = 1024 * 1024;

/// Languages with syntax support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SynLang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    /// Markdown headings (no tree-sitter needed).
    Markdown,
}

impl SynLang {
    pub fn name(self) -> &'static str {
        match self {
            SynLang::Rust => "rust",
            SynLang::Python => "python",
            SynLang::JavaScript => "javascript",
            SynLang::TypeScript => "typescript",
            SynLang::Tsx => "tsx",
            SynLang::Go => "go",
            SynLang::Java => "java",
            SynLang::C => "c",
            SynLang::Cpp => "cpp",
            SynLang::Ruby => "ruby",
            SynLang::Markdown => "markdown",
        }
    }

    /// True for programming languages (not docs).
    pub fn is_code(self) -> bool {
        self != SynLang::Markdown
    }
}

/// Map a path to a syntax-supported language by extension. Returns `None`
/// for unsupported languages (and for every code language when the `syntax`
/// feature is disabled).
pub fn lang_for_path(path: &str) -> Option<SynLang> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let lang = match ext {
        "md" | "markdown" => SynLang::Markdown,
        "rs" => SynLang::Rust,
        "py" | "pyi" => SynLang::Python,
        "js" | "mjs" | "cjs" | "jsx" => SynLang::JavaScript,
        "ts" | "mts" | "cts" => SynLang::TypeScript,
        "tsx" => SynLang::Tsx,
        "go" => SynLang::Go,
        "java" => SynLang::Java,
        "c" => SynLang::C,
        // `.h` is parsed as C++: the C++ grammar accepts nearly all C headers
        // and many `.h` files are C++.
        "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "c++" | "h++" | "ipp" | "inl" => {
            SynLang::Cpp
        }
        "rb" | "rake" | "gemspec" => SynLang::Ruby,
        _ => return None,
    };
    if lang.is_code() && !cfg!(feature = "syntax") {
        return None;
    }
    Some(lang)
}

/// Languages with syntax support in this build (for messages).
pub fn supported_languages() -> &'static str {
    if cfg!(feature = "syntax") {
        "Rust, Python, JavaScript/JSX, TypeScript/TSX, Go, Java, C, C++, Ruby (and Markdown headings)"
    } else {
        "Markdown headings only (xgrep was built without the `syntax` feature)"
    }
}

/// Symbol kinds (stored as `u8` in the sidecar cache; append only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Function = 0,
    Method = 1,
    Class = 2,
    Struct = 3,
    Enum = 4,
    Interface = 5,
    Trait = 6,
    Type = 7,
    Impl = 8,
    Module = 9,
    Macro = 10,
    Constant = 11,
    Variable = 12,
    /// Function prototype / forward declaration (C, C++).
    Declaration = 13,
    Heading = 14,
    Constructor = 15,
    Union = 16,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        use Kind::*;
        Some(match v {
            0 => Function,
            1 => Method,
            2 => Class,
            3 => Struct,
            4 => Enum,
            5 => Interface,
            6 => Trait,
            7 => Type,
            8 => Impl,
            9 => Module,
            10 => Macro,
            11 => Constant,
            12 => Variable,
            13 => Declaration,
            14 => Heading,
            15 => Constructor,
            16 => Union,
            _ => return None,
        })
    }

    /// Short label used in outputs.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Function => "fn",
            Kind::Method => "method",
            Kind::Class => "class",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Interface => "interface",
            Kind::Trait => "trait",
            Kind::Type => "type",
            Kind::Impl => "impl",
            Kind::Module => "mod",
            Kind::Macro => "macro",
            Kind::Constant => "const",
            Kind::Variable => "var",
            Kind::Declaration => "decl",
            Kind::Heading => "heading",
            Kind::Constructor => "constructor",
            Kind::Union => "union",
        }
    }

    /// Code that runs (a caller for `find_references`).
    pub fn is_callable(self) -> bool {
        matches!(
            self,
            Kind::Function | Kind::Method | Kind::Constructor | Kind::Macro
        )
    }

    /// Kinds that contain methods.
    pub fn is_container(self) -> bool {
        matches!(
            self,
            Kind::Class
                | Kind::Struct
                | Kind::Enum
                | Kind::Interface
                | Kind::Trait
                | Kind::Impl
                | Kind::Module
                | Kind::Union
        )
    }

    /// Score for `find_definitions` ranking (same scale as the regex backend).
    pub fn def_score(self) -> i32 {
        match self {
            Kind::Function
            | Kind::Method
            | Kind::Constructor
            | Kind::Class
            | Kind::Struct
            | Kind::Enum
            | Kind::Interface
            | Kind::Trait
            | Kind::Type
            | Kind::Union
            | Kind::Macro => 100,
            Kind::Constant => 80,
            Kind::Variable => 60,
            Kind::Impl => 60,
            Kind::Module => 50,
            Kind::Declaration => 50,
            Kind::Heading => 0,
        }
    }
}

/// One definition in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub kind: Kind,
    pub name: String,
    /// 1-based line of the name.
    pub line: u32,
    /// 1-based first line of the whole definition (may include annotations).
    pub start: u32,
    /// 1-based last line of the definition.
    pub end: u32,
    /// Index of the enclosing symbol in [`FileTags::defs`], if any.
    pub parent: Option<u32>,
    /// Signature: definition text up to its body, whitespace collapsed.
    pub sig: String,
}

/// Syntax facts for one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTags {
    /// Definitions ordered by start line (parents before children).
    pub defs: Vec<Symbol>,
    /// Identifier usages that are not definition names: (name, count).
    pub refs: Vec<(String, u32)>,
    /// Number of lines in the file.
    pub lines: u32,
}

impl FileTags {
    /// Depth of a symbol (0 = top level).
    pub fn depth(&self, idx: usize) -> usize {
        let mut d = 0;
        let mut cur = self.defs[idx].parent;
        while let Some(p) = cur {
            d += 1;
            cur = self.defs.get(p as usize).and_then(|s| s.parent);
            if d > 64 {
                break;
            }
        }
        d
    }

    /// Smallest definition whose range contains `line`, filtered by `pred`.
    pub fn enclosing(&self, line: u32, pred: impl Fn(&Symbol) -> bool) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, s) in self.defs.iter().enumerate() {
            if s.start <= line && line <= s.end && pred(s) {
                match best {
                    Some(b) if self.defs[b].end - self.defs[b].start <= s.end - s.start => {}
                    _ => best = Some(i),
                }
            }
        }
        best
    }

    /// Qualified display name: `Parent.name` when the parent is a container.
    pub fn qualified(&self, idx: usize) -> String {
        let s = &self.defs[idx];
        match s.parent.map(|p| &self.defs[p as usize]) {
            Some(p) if p.kind.is_container() && p.kind != Kind::Module && !p.name.is_empty() => {
                format!("{}.{}", p.name, s.name)
            }
            _ => s.name.clone(),
        }
    }
}

/// Fix parent links and method kinds after collecting defs in any order,
/// using line ranges for containment.
pub fn finalize_defs(defs: &mut Vec<Symbol>) {
    let items: Vec<(Symbol, u64, u64)> = std::mem::take(defs)
        .into_iter()
        .map(|s| {
            let (a, b) = ((s.start as u64) << 32, ((s.end as u64) << 32) | 0xffff_ffff);
            (s, a, b)
        })
        .collect();
    *defs = finalize_spans(items);
}

/// Like [`finalize_defs`] with explicit `[start, end)` spans (byte offsets
/// for parsed files, so definitions on one line still nest correctly).
/// Sorts by (start, larger span first) and links each def to the smallest
/// def whose span contains it; functions directly inside a type become
/// methods.
pub fn finalize_spans(mut items: Vec<(Symbol, u64, u64)>) -> Vec<Symbol> {
    items.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then(b.2.cmp(&a.2))
            .then(a.0.line.cmp(&b.0.line))
            .then(a.0.name.cmp(&b.0.name))
    });
    items.dedup_by(|b, a| a.0.name == b.0.name && a.1 == b.1 && a.2 == b.2);
    let mut defs: Vec<Symbol> = Vec::with_capacity(items.len());
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..items.len() {
        while let Some(&top) = stack.last() {
            if items[top].1 <= items[i].1 && items[i].2 <= items[top].2 {
                break;
            }
            stack.pop();
        }
        let mut s = items[i].0.clone();
        s.parent = stack.last().map(|&p| p as u32);
        if s.kind == Kind::Function {
            if let Some(&p) = stack.last() {
                if defs[p].kind.is_container() && defs[p].kind != Kind::Module {
                    s.kind = Kind::Method;
                }
            }
        }
        defs.push(s);
        stack.push(i);
    }
    defs
}

/// Markdown headings as an outline (ATX `#` headings outside code fences).
pub fn markdown_tags(src: &str) -> FileTags {
    let lines: Vec<&str> = src.lines().collect();
    let mut heads: Vec<(usize, u32, String)> = Vec::new(); // (level, line, text)
    let mut fence: Option<&str> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if t.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        if l.starts_with('#') {
            let level = l.chars().take_while(|&c| c == '#').count();
            let rest = &l[level..];
            if level <= 6 && (rest.is_empty() || rest.starts_with(' ')) {
                let text = rest.trim().trim_end_matches('#').trim().to_string();
                heads.push((level, i as u32 + 1, text));
            }
        }
    }
    let total = lines.len() as u32;
    let mut defs = Vec::with_capacity(heads.len());
    for (k, (level, line, text)) in heads.iter().enumerate() {
        let end = heads[k + 1..]
            .iter()
            .find(|(l2, _, _)| l2 <= level)
            .map(|(_, l, _)| l - 1)
            .unwrap_or(total)
            .max(*line);
        defs.push(Symbol {
            kind: Kind::Heading,
            name: text.clone(),
            line: *line,
            start: *line,
            end,
            parent: None,
            sig: format!("{} {}", "#".repeat(*level), text),
        });
    }
    finalize_defs(&mut defs);
    FileTags {
        defs,
        refs: Vec::new(),
        lines: total,
    }
}

/// Compute tags for a file's source text. `None` when the language is not
/// supported in this build.
pub fn tags_for_source(lang: SynLang, src: &str) -> Option<FileTags> {
    match lang {
        SynLang::Markdown => Some(markdown_tags(src)),
        #[cfg(feature = "syntax")]
        _ => parse::parse_tags(lang, src),
        #[cfg(not(feature = "syntax"))]
        _ => None,
    }
}

/// Collapse whitespace runs and cap the length of a signature.
#[cfg_attr(not(feature = "syntax"), allow(dead_code))]
pub fn clean_sig(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars + 8));
    let mut prev_space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim_end_matches(['{', ':', ' ']).trim_end().to_string();
    if out.chars().count() > max_chars {
        let cut: String = out.chars().take(max_chars).collect();
        format!("{}…", cut)
    } else {
        out
    }
}

/// Display form of a symbol for outlines and maps: the signature, prefixed
/// with the kind label when the signature does not start with a keyword.
pub fn display_sig(s: &Symbol) -> String {
    if s.sig.is_empty() {
        return format!("{} {}", s.kind.label(), s.name);
    }
    if s.kind == Kind::Heading {
        return s.sig.clone();
    }
    if s.sig.starts_with(&s.name) {
        format!("{} {}", s.kind.label(), s.sig)
    } else {
        s.sig.clone()
    }
}

/// `L10` or `L10-40`.
pub fn line_span(start: u32, end: u32) -> String {
    if start == end {
        format!("L{}", start)
    } else {
        format!("L{}-{}", start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, kind: Kind, start: u32, end: u32) -> Symbol {
        Symbol {
            kind,
            name: name.into(),
            line: start,
            start,
            end,
            parent: None,
            sig: String::new(),
        }
    }

    #[test]
    fn finalize_links_parents_and_methods() {
        let mut d = vec![
            sym("m", Kind::Function, 3, 4),
            sym("C", Kind::Class, 1, 10),
            sym("f", Kind::Function, 12, 14),
        ];
        finalize_defs(&mut d);
        assert_eq!(d[0].name, "C");
        assert_eq!(d[1].name, "m");
        assert_eq!(d[1].parent, Some(0));
        assert_eq!(d[1].kind, Kind::Method);
        assert_eq!(d[2].parent, None);
        assert_eq!(d[2].kind, Kind::Function);
    }

    #[test]
    fn enclosing_picks_smallest() {
        let mut d = vec![sym("C", Kind::Class, 1, 10), sym("m", Kind::Function, 3, 6)];
        finalize_defs(&mut d);
        let t = FileTags {
            defs: d,
            refs: vec![],
            lines: 10,
        };
        assert_eq!(t.enclosing(4, |_| true), Some(1));
        assert_eq!(t.enclosing(8, |_| true), Some(0));
        assert_eq!(t.enclosing(11, |_| true), None);
        assert_eq!(t.qualified(1), "C.m");
    }

    #[test]
    fn markdown_headings() {
        let t = markdown_tags("# A\ntext\n## B\n```\n# not\n```\n# C\n");
        let names: Vec<&str> = t.defs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
        assert_eq!((t.defs[0].start, t.defs[0].end), (1, 6));
        assert_eq!(t.defs[1].parent, Some(0));
    }

    #[test]
    fn clean_sig_collapses() {
        assert_eq!(
            clean_sig("fn  a(\n  x: u32,\n) -> u32 {", 100),
            "fn a( x: u32, ) -> u32"
        );
        assert_eq!(clean_sig("abcdef", 3), "abc…");
    }

    #[test]
    fn lang_detection() {
        assert_eq!(lang_for_path("a/b.md"), Some(SynLang::Markdown));
        assert_eq!(lang_for_path("x.unknown"), None);
        if cfg!(feature = "syntax") {
            assert_eq!(lang_for_path("a.rs"), Some(SynLang::Rust));
            assert_eq!(lang_for_path("a.tsx"), Some(SynLang::Tsx));
            assert_eq!(lang_for_path("a.h"), Some(SynLang::Cpp));
        } else {
            assert_eq!(lang_for_path("a.rs"), None);
        }
    }
}
