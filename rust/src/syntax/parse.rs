//! tree-sitter parsing: definitions from the grammars' `tags.scm` queries
//! (plus a few extra patterns for things the shipped queries leave out, such
//! as Rust `impl` blocks, constants, Java constructors and TS type aliases),
//! and identifier usages from a walk over the syntax tree.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

use super::{clean_sig, finalize_spans, FileTags, Kind, Symbol, SynLang};

/// Max chars of a signature.
const SIG_CHARS: usize = 200;

const RUST_EXTRA: &str = r#"
(impl_item type: (_) @name) @definition.impl
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(function_signature_item name: (identifier) @name) @definition.method
(macro_definition name: (identifier) @name) @definition.macro
"#;

const PYTHON_EXTRA: &str = "";

const JS_EXTRA: &str = r#"
(method_definition name: (property_identifier) @name) @definition.method
(program (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)))
(program (export_statement (function_declaration name: (identifier) @name) @definition.function))
"#;

const TS_EXTRA: &str = r#"
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(internal_module name: (_) @name) @definition.module
(class_declaration name: (type_identifier) @name) @definition.class
"#;

const GO_EXTRA: &str = r#"
(source_file (const_declaration (const_spec name: (identifier) @name) @definition.constant))
(source_file (var_declaration (var_spec name: (identifier) @name) @definition.variable))
"#;

const JAVA_EXTRA: &str = r#"
(enum_declaration name: (identifier) @name) @definition.enum
(record_declaration name: (identifier) @name) @definition.class
(constructor_declaration name: (identifier) @name) @definition.constructor
(annotation_type_declaration name: (identifier) @name) @definition.interface
"#;

const C_EXTRA: &str = r#"
(preproc_def name: (identifier) @name) @definition.macro
(preproc_function_def name: (identifier) @name) @definition.macro
(union_specifier name: (type_identifier) @name body: (_)) @definition.class
(enum_specifier name: (type_identifier) @name body: (_)) @definition.enum
"#;

const CPP_EXTRA: &str = r#"
(preproc_def name: (identifier) @name) @definition.macro
(preproc_function_def name: (identifier) @name) @definition.macro
(namespace_definition name: (_) @name) @definition.module
(function_declarator declarator: (qualified_identifier name: (_) @name)) @definition.function
(union_specifier name: (type_identifier) @name body: (_)) @definition.class
(enum_specifier name: (type_identifier) @name body: (_)) @definition.enum
"#;

const RUBY_EXTRA: &str = "";

/// Identifier-like leaf node kinds collected as references.
const IDENT_KINDS: &[&str] = &[
    "identifier",
    "type_identifier",
    "field_identifier",
    "property_identifier",
    "shorthand_property_identifier",
    "constant",
    "namespace_identifier",
];

struct LangDef {
    language: Language,
    query: Option<Query>,
    /// capture index -> definition kind (None for non-definition captures)
    def_kinds: Vec<Option<&'static str>>,
    name_idx: Option<u32>,
}

fn language(lang: SynLang) -> Option<Language> {
    Some(match lang {
        SynLang::Rust => tree_sitter_rust::LANGUAGE.into(),
        SynLang::Python => tree_sitter_python::LANGUAGE.into(),
        SynLang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        SynLang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        SynLang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        SynLang::Go => tree_sitter_go::LANGUAGE.into(),
        SynLang::Java => tree_sitter_java::LANGUAGE.into(),
        SynLang::C => tree_sitter_c::LANGUAGE.into(),
        SynLang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        SynLang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        SynLang::Markdown => return None,
    })
}

/// (shipped tags query parts, extra patterns)
fn query_sources(lang: SynLang) -> (Vec<&'static str>, &'static str) {
    match lang {
        SynLang::Rust => (vec![tree_sitter_rust::TAGS_QUERY], RUST_EXTRA),
        SynLang::Python => (vec![tree_sitter_python::TAGS_QUERY], PYTHON_EXTRA),
        SynLang::JavaScript => (vec![tree_sitter_javascript::TAGS_QUERY], JS_EXTRA),
        SynLang::TypeScript | SynLang::Tsx => (
            vec![
                tree_sitter_javascript::TAGS_QUERY,
                tree_sitter_typescript::TAGS_QUERY,
            ],
            // The TS grammar extends JS, so the JS extras apply too.
            TS_EXTRA,
        ),
        SynLang::Go => (vec![tree_sitter_go::TAGS_QUERY], GO_EXTRA),
        SynLang::Java => (vec![tree_sitter_java::TAGS_QUERY], JAVA_EXTRA),
        SynLang::C => (vec![tree_sitter_c::TAGS_QUERY], C_EXTRA),
        SynLang::Cpp => (vec![tree_sitter_cpp::TAGS_QUERY], CPP_EXTRA),
        SynLang::Ruby => (vec![tree_sitter_ruby::TAGS_QUERY], RUBY_EXTRA),
        SynLang::Markdown => (vec![], ""),
    }
}

/// Build the query for a language: shipped tags + extras. Patterns without a
/// `definition.*` capture (the `reference.*` ones) are disabled because
/// references come from the identifier walk.
pub(crate) fn build_query(language: &Language, lang: SynLang) -> Result<Query, String> {
    let (tags, extra) = query_sources(lang);
    let mut src = tags.join("\n");
    if matches!(lang, SynLang::TypeScript | SynLang::Tsx) {
        src.push('\n');
        src.push_str(JS_EXTRA);
    }
    src.push('\n');
    src.push_str(extra);
    let mut q = Query::new(language, &src).map_err(|e| format!("{:?}: {}", lang, e))?;
    let def_caps: Vec<bool> = q
        .capture_names()
        .iter()
        .map(|n| n.starts_with("definition."))
        .collect();
    for p in 0..q.pattern_count() {
        let has_def = q
            .capture_quantifiers(p)
            .iter()
            .enumerate()
            .any(|(i, qn)| def_caps[i] && *qn != tree_sitter::CaptureQuantifier::Zero);
        if !has_def {
            q.disable_pattern(p);
        }
    }
    Ok(q)
}

fn lang_def(lang: SynLang) -> Option<&'static LangDef> {
    static DEFS: OnceLock<HashMap<SynLang, OnceLock<Option<LangDef>>>> = OnceLock::new();
    let map = DEFS.get_or_init(|| {
        [
            SynLang::Rust,
            SynLang::Python,
            SynLang::JavaScript,
            SynLang::TypeScript,
            SynLang::Tsx,
            SynLang::Go,
            SynLang::Java,
            SynLang::C,
            SynLang::Cpp,
            SynLang::Ruby,
        ]
        .into_iter()
        .map(|l| (l, OnceLock::new()))
        .collect()
    });
    map.get(&lang)?
        .get_or_init(|| {
            let language = language(lang)?;
            let query = build_query(&language, lang).ok();
            let (def_kinds, name_idx) = match &query {
                Some(q) => (
                    q.capture_names()
                        .iter()
                        .map(|n| n.strip_prefix("definition.").map(static_kind))
                        .collect(),
                    q.capture_index_for_name("name"),
                ),
                None => (Vec::new(), None),
            };
            Some(LangDef {
                language,
                query,
                def_kinds,
                name_idx,
            })
        })
        .as_ref()
}

/// Intern a `definition.<kind>` suffix.
fn static_kind(k: &str) -> &'static str {
    const KINDS: &[&str] = &[
        "function",
        "method",
        "class",
        "interface",
        "module",
        "macro",
        "constant",
        "variable",
        "type",
        "enum",
        "impl",
        "struct",
        "constructor",
    ];
    KINDS.iter().find(|&&x| x == k).copied().unwrap_or("other")
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

/// Parse `src` and extract definitions and identifier usages.
pub fn parse_tags(lang: SynLang, src: &str) -> Option<FileTags> {
    let ld = lang_def(lang)?;
    let tree = PARSER.with(|p| {
        let mut p = p.borrow_mut();
        p.set_language(&ld.language).ok()?;
        p.parse(src, None)
    })?;
    let bytes = src.as_bytes();
    let root = tree.root_node();
    let mut def_name_bytes: HashSet<usize> = HashSet::new();
    // node id -> symbol; when several patterns capture the same node the
    // more specific kind wins (a function-valued const is a function).
    let mut by_node: HashMap<usize, (Symbol, u64, u64)> = HashMap::new();
    if let (Some(q), Some(name_idx)) = (&ld.query, ld.name_idx) {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(q, root, bytes);
        while let Some(m) = matches.next() {
            let mut def: Option<(Node, &str)> = None;
            let mut name: Option<Node> = None;
            for c in m.captures {
                if c.index == name_idx {
                    name = Some(c.node);
                } else if let Some(Some(k)) = ld.def_kinds.get(c.index as usize) {
                    def = Some((c.node, k));
                }
            }
            let (Some((node, kind)), Some(name)) = (def, name) else {
                continue;
            };
            if let Some((sym, a, b)) = make_symbol(lang, node, kind, name, src) {
                def_name_bytes.insert(name.start_byte());
                match by_node.get(&node.id()) {
                    Some(old) if priority(old.0.kind) >= priority(sym.kind) => {}
                    _ => {
                        by_node.insert(node.id(), (sym, a, b));
                    }
                }
            }
        }
    }
    let defs = finalize_spans(by_node.into_values().collect());
    let refs = collect_refs(root, bytes, &def_name_bytes);
    let lines = src.lines().count() as u32;
    Some(FileTags { defs, refs, lines })
}

/// Preference when two patterns capture the same node.
fn priority(k: Kind) -> u8 {
    match k {
        Kind::Variable => 0,
        Kind::Constant => 1,
        _ => 2,
    }
}

fn text<'a>(n: Node, src: &'a str) -> &'a str {
    src.get(n.start_byte()..n.end_byte()).unwrap_or("")
}

/// Kind from the node type, falling back to the tags capture suffix.
fn kind_for(lang: SynLang, node_kind: &str, capture: &str) -> Kind {
    match node_kind {
        "struct_item" | "struct_specifier" | "record_declaration" => return Kind::Struct,
        "enum_item" | "enum_specifier" | "enum_declaration" => return Kind::Enum,
        "union_item" | "union_specifier" => return Kind::Union,
        "type_item" | "type_definition" | "type_alias_declaration" => return Kind::Type,
        "trait_item" => return Kind::Trait,
        "mod_item" | "namespace_definition" | "internal_module" | "module" => return Kind::Module,
        "impl_item" => return Kind::Impl,
        "macro_definition" | "preproc_def" | "preproc_function_def" => return Kind::Macro,
        "const_item" | "static_item" => return Kind::Constant,
        "constructor_declaration" => return Kind::Constructor,
        "method_declaration" if lang == SynLang::Go => return Kind::Method,
        "class_specifier" | "class_declaration" | "class_definition" | "class" => {
            return Kind::Class
        }
        "interface_declaration" => return Kind::Interface,
        _ => {}
    }
    match capture {
        "function" | "method" => Kind::Function,
        "class" => Kind::Class,
        "interface" => Kind::Interface,
        "module" => Kind::Module,
        "macro" => Kind::Macro,
        "constant" => Kind::Constant,
        "variable" => Kind::Variable,
        "type" => Kind::Type,
        "enum" => Kind::Enum,
        "impl" => Kind::Impl,
        "struct" => Kind::Struct,
        "constructor" => Kind::Constructor,
        _ => Kind::Function,
    }
}

fn make_symbol(
    lang: SynLang,
    node: Node,
    capture: &str,
    name: Node,
    src: &str,
) -> Option<(Symbol, u64, u64)> {
    let mut range = node;
    let mut kind = kind_for(lang, node.kind(), capture);
    // C/C++: the tags query captures the `function_declarator`; widen to the
    // whole definition, or mark prototypes as declarations.
    if node.kind() == "function_declarator" {
        let mut cur = node;
        while let Some(p) = cur.parent() {
            match p.kind() {
                "pointer_declarator"
                | "reference_declarator"
                | "function_declarator"
                | "parenthesized_declarator" => cur = p,
                _ => break,
            }
        }
        match cur.parent() {
            Some(p) if p.kind() == "function_definition" => {
                range = p;
                if let Some(t) = p.parent().filter(|t| t.kind() == "template_declaration") {
                    range = t;
                }
            }
            Some(p) if p.kind() == "declaration" || p.kind() == "field_declaration" => {
                range = p;
                kind = Kind::Declaration;
            }
            _ => return None,
        }
    }
    // JS/TS: include the `export` keyword in range and signature.
    if let Some(p) = range.parent().filter(|p| p.kind() == "export_statement") {
        range = p;
    }
    let mut name_text = text(name, src).trim().to_string();
    if name_text.contains("::") && kind != Kind::Impl {
        // C++ `ns::W::set` -> `set`
        name_text = name_text.rsplit("::").next().unwrap_or("").to_string();
    }
    if kind == Kind::Impl {
        // `impl<T> Trait for path::Type<T>` -> `Type`
        let base = name_text.split('<').next().unwrap_or("").trim();
        name_text = base.rsplit("::").next().unwrap_or(base).to_string();
    }
    if name_text.is_empty() {
        return None;
    }
    if kind == Kind::Function && name_text == "constructor" && node.kind() == "method_definition" {
        kind = Kind::Constructor;
    }
    let sig = signature(range, src);
    // A node ending at column 0 ends on the previous line.
    let mut end = range.end_position().row as u32 + 1;
    if range.end_position().column == 0 && range.end_position().row > range.start_position().row {
        end -= 1;
    }
    Some((
        Symbol {
            kind,
            name: name_text,
            line: name.start_position().row as u32 + 1,
            start: range.start_position().row as u32 + 1,
            end,
            parent: None,
            sig,
        },
        range.start_byte() as u64,
        range.end_byte() as u64,
    ))
}

/// Definition text up to its body (or its first line when it has none).
fn signature(node: Node, src: &str) -> String {
    let inner = if node.kind() == "template_declaration" {
        node.named_child(node.named_child_count().saturating_sub(1))
            .unwrap_or(node)
    } else {
        node
    };
    let body = inner
        .child_by_field_name("body")
        .or_else(|| {
            inner
                .child_by_field_name("value")
                .and_then(|v| v.child_by_field_name("body"))
        })
        .filter(|b| b.start_byte() > node.start_byte());
    let start = node.start_byte();
    let full = text(node, src);
    let raw = match body {
        Some(b) => src.get(start..b.start_byte()).unwrap_or(full),
        None => full.lines().next().unwrap_or(""),
    };
    // Bound the work for pathological signatures.
    let raw = if raw.len() > 2000 {
        let mut cut = 2000;
        while !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        &raw[..cut]
    } else {
        raw
    };
    // Drop line comments inside multi-line signatures.
    let mut joined = String::with_capacity(raw.len());
    for l in raw.lines() {
        let t = l.trim();
        if t.starts_with("//") && !joined.is_empty() {
            continue;
        }
        joined.push_str(l);
        joined.push(' ');
    }
    clean_sig(&joined, SIG_CHARS)
}

fn is_scoped(kind: &str) -> bool {
    matches!(
        kind,
        "scoped_identifier" | "scoped_type_identifier" | "scoped_use_list"
    )
}

/// True when `n` is (the last segment of) the `path` of a scoped path.
fn in_path_position(n: Node) -> bool {
    let Some(p) = n.parent().filter(|p| is_scoped(p.kind())) else {
        return false;
    };
    if p.child_by_field_name("path")
        .is_some_and(|c| c.id() == n.id())
    {
        return true;
    }
    p.parent().is_some_and(|pp| {
        is_scoped(pp.kind())
            && pp
                .child_by_field_name("path")
                .is_some_and(|c| c.id() == p.id())
    })
}

/// Count identifier leaves that are not definition names.
fn collect_refs(root: Node, bytes: &[u8], def_names: &HashSet<usize>) -> Vec<(String, u32)> {
    let mut counts: HashMap<&[u8], u32> = HashMap::new();
    let mut cursor = root.walk();
    'outer: loop {
        let n = cursor.node();
        if n.child_count() == 0 {
            // `core::fmt`, `std::io`: lowercase path segments name modules
            // or crates, not symbols (`Foo::new` keeps `Foo`).
            let module_path = bytes
                .get(n.start_byte())
                .is_some_and(|b| b.is_ascii_lowercase())
                && in_path_position(n);
            if IDENT_KINDS.contains(&n.kind())
                && !module_path
                && !def_names.contains(&n.start_byte())
            {
                if let Some(b) = bytes.get(n.start_byte()..n.end_byte()) {
                    if b.len() >= 2 && b.len() <= 80 {
                        *counts.entry(b).or_insert(0) += 1;
                    }
                }
            }
        } else if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'outer;
            }
        }
    }
    let mut out: Vec<(String, u32)> = counts
        .into_iter()
        .filter_map(|(k, v)| std::str::from_utf8(k).ok().map(|s| (s.to_string(), v)))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(t: &FileTags) -> Vec<(String, &'static str, u32, u32, u32)> {
        t.defs
            .iter()
            .map(|s| (s.name.clone(), s.kind.label(), s.line, s.start, s.end))
            .collect()
    }

    fn has(t: &FileTags, name: &str, kind: &str) -> bool {
        t.defs
            .iter()
            .any(|s| s.name == name && s.kind.label() == kind)
    }

    #[test]
    fn all_queries_compile() {
        for l in [
            SynLang::Rust,
            SynLang::Python,
            SynLang::JavaScript,
            SynLang::TypeScript,
            SynLang::Tsx,
            SynLang::Go,
            SynLang::Java,
            SynLang::C,
            SynLang::Cpp,
            SynLang::Ruby,
        ] {
            let lang = language(l).unwrap();
            if let Err(e) = build_query(&lang, l) {
                panic!("query for {:?} does not compile: {}", l, e);
            }
        }
    }

    #[test]
    fn rust_defs() {
        let src = "pub struct Foo { x: u32 }\n\nimpl<T> Display for Foo<T> {\n    fn fmt(&self) -> u32 {\n        helper(1)\n    }\n}\n\npub const MAX: usize = 3;\n\npub trait Tr {\n    fn req(&self);\n}\n\nmacro_rules! mm { () => {} }\n\nfn helper(\n    a: u32,\n) -> u32 {\n    a\n}\n";
        let t = parse_tags(SynLang::Rust, src).unwrap();
        assert!(has(&t, "Foo", "struct"), "{:?}", names(&t));
        assert!(has(&t, "Foo", "impl"), "{:?}", names(&t));
        assert!(has(&t, "fmt", "method"), "{:?}", names(&t));
        assert!(has(&t, "MAX", "const"));
        assert!(has(&t, "Tr", "trait"));
        assert!(has(&t, "req", "method"));
        assert!(has(&t, "mm", "macro"));
        let h = t.defs.iter().find(|s| s.name == "helper").unwrap();
        assert_eq!(h.kind, Kind::Function);
        assert_eq!((h.start, h.end), (17, 21));
        assert_eq!(h.sig, "fn helper(a: u32) -> u32");
        let imp = t.defs.iter().find(|s| s.kind == Kind::Impl).unwrap();
        assert_eq!(imp.sig, "impl<T> Display for Foo<T>");
        // refs: helper is used once (call), Display referenced
        assert!(t.refs.iter().any(|(n, c)| n == "helper" && *c == 1));
        assert!(t.refs.iter().any(|(n, _)| n == "Display"));
        // module path segments are not references; type paths are
        let t = parse_tags(
            SynLang::Rust,
            "fn f() { core::mem::swap(a, b); Foo::new(); }\n",
        )
        .unwrap();
        let names: Vec<&str> = t.refs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"core") && !names.contains(&"mem"),
            "{:?}",
            names
        );
        assert!(
            names.contains(&"Foo") && names.contains(&"swap"),
            "{:?}",
            names
        );
    }

    #[test]
    fn python_defs() {
        let src = "X = 1\n\nclass A(Base):\n    @property\n    def m(self):\n        return go()\n\ndef go():\n    pass\n";
        let t = parse_tags(SynLang::Python, src).unwrap();
        assert!(has(&t, "X", "const"));
        assert!(has(&t, "A", "class"));
        assert!(has(&t, "m", "method"));
        assert!(has(&t, "go", "fn"));
        let a = t.defs.iter().find(|s| s.name == "A").unwrap();
        assert_eq!(a.sig, "class A(Base)");
    }

    #[test]
    fn js_ts_defs() {
        let src = "export function top(a) { return a }\nconst arrow = (x) => x;\nexport const LIMIT = 5;\nclass K {\n  constructor() {}\n  run() { top(1) }\n}\n";
        let t = parse_tags(SynLang::JavaScript, src).unwrap();
        assert!(has(&t, "top", "fn"), "{:?}", names(&t));
        assert!(has(&t, "arrow", "fn"), "{:?}", names(&t));
        assert!(has(&t, "LIMIT", "const"), "{:?}", names(&t));
        assert!(has(&t, "K", "class"));
        assert!(has(&t, "run", "method"));
        assert!(has(&t, "constructor", "constructor"), "{:?}", names(&t));

        let ts = "export interface P { a: string }\ntype Id = string;\nenum E { A }\nnamespace NS { export function f() {} }\nexport class S { go(): void {} }\n";
        for l in [SynLang::TypeScript, SynLang::Tsx] {
            let t = parse_tags(l, ts).unwrap();
            assert!(has(&t, "P", "interface"), "{:?}", names(&t));
            assert!(has(&t, "Id", "type"), "{:?}", names(&t));
            assert!(has(&t, "E", "enum"), "{:?}", names(&t));
            assert!(has(&t, "NS", "mod"), "{:?}", names(&t));
            assert!(has(&t, "S", "class"), "{:?}", names(&t));
            assert!(has(&t, "go", "method"), "{:?}", names(&t));
        }
    }

    #[test]
    fn go_defs() {
        let src = "package p\n\nconst Max = 3\n\ntype Server struct {\n\tx int\n}\n\ntype Doer interface{ Do() }\n\nfunc (s *Server) Handle(w int) {\n\thelp()\n}\n\nfunc help() {}\n";
        let t = parse_tags(SynLang::Go, src).unwrap();
        assert!(has(&t, "Max", "const"), "{:?}", names(&t));
        assert!(has(&t, "Server", "type"), "{:?}", names(&t));
        assert!(has(&t, "Handle", "method"));
        assert!(has(&t, "help", "fn"));
        let h = t.defs.iter().find(|s| s.name == "Handle").unwrap();
        assert_eq!(h.sig, "func (s *Server) Handle(w int)");
    }

    #[test]
    fn java_defs() {
        let src = "public class Svc {\n  public Svc() {}\n  @Override\n  public void run(int a) {\n    helper();\n  }\n}\nenum Color { RED }\ninterface I { void x(); }\n";
        let t = parse_tags(SynLang::Java, src).unwrap();
        assert!(has(&t, "Svc", "class"), "{:?}", names(&t));
        assert!(has(&t, "Svc", "constructor"), "{:?}", names(&t));
        assert!(has(&t, "run", "method"));
        assert!(has(&t, "Color", "enum"));
        assert!(has(&t, "I", "interface"));
        let r = t.defs.iter().find(|s| s.name == "run").unwrap();
        assert_eq!(r.line, 4);
        assert_eq!(r.start, 3);
    }

    #[test]
    fn c_cpp_defs() {
        let src = "#define MAXV 10\nstruct node { int v; };\nint compute(int a);\nstatic int compute(int a) {\n  return a;\n}\ntypedef struct node node_t;\n";
        let t = parse_tags(SynLang::C, src).unwrap();
        assert!(has(&t, "MAXV", "macro"), "{:?}", names(&t));
        assert!(has(&t, "node", "struct"), "{:?}", names(&t));
        assert!(has(&t, "compute", "decl"), "{:?}", names(&t));
        assert!(has(&t, "compute", "fn"), "{:?}", names(&t));
        assert!(has(&t, "node_t", "type"), "{:?}", names(&t));
        let f = t
            .defs
            .iter()
            .find(|s| s.name == "compute" && s.kind == Kind::Function)
            .unwrap();
        assert_eq!((f.start, f.end), (4, 6));

        let cpp = "namespace ns {\nclass W {\n public:\n  int get() const { return 1; }\n  void set(int v);\n};\n}\nvoid ns::W::set(int v) {}\n";
        let t = parse_tags(SynLang::Cpp, cpp).unwrap();
        assert!(has(&t, "ns", "mod"), "{:?}", names(&t));
        assert!(has(&t, "W", "class"), "{:?}", names(&t));
        assert!(has(&t, "get", "method"), "{:?}", names(&t));
        assert!(has(&t, "set", "decl"), "{:?}", names(&t));
        assert!(
            t.defs
                .iter()
                .any(|s| s.name == "set" && s.kind == Kind::Function && s.start == 8),
            "{:?}",
            names(&t)
        );
    }

    #[test]
    fn ruby_defs() {
        let src = "module M\n  class C < B\n    def go(a)\n      a\n    end\n    def self.make\n    end\n  end\nend\n";
        let t = parse_tags(SynLang::Ruby, src).unwrap();
        assert!(has(&t, "M", "mod"), "{:?}", names(&t));
        assert!(has(&t, "C", "class"), "{:?}", names(&t));
        assert!(has(&t, "go", "method"), "{:?}", names(&t));
        assert!(has(&t, "make", "method"), "{:?}", names(&t));
    }

    #[test]
    fn broken_source_still_yields_defs() {
        let src = "fn ok() {}\nfn broken( {\n";
        let t = parse_tags(SynLang::Rust, src).unwrap();
        assert!(has(&t, "ok", "fn"));
    }
}
