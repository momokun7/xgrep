//! Symbol definition detection used by the MCP `find_definitions` tool and by
//! result ranking.
//!
//! The flow is:
//!
//! 1. [`DefinitionBackend::prefilter`] gives a regex run through the trigram
//!    index (every line that mentions the symbol as a word).
//! 2. Candidate files with syntax support (Rust, Python, JS/TS, Go, Java,
//!    C/C++, Ruby) are parsed with tree-sitter and classified from their tags
//!    (`crate::syntax::tools::classify_definitions`, cached in `<index>.tags`).
//! 3. Other files fall back to [`CompiledDefs::classify`], the regex
//!    heuristics below, which decide per line and per language whether the
//!    line *defines* the symbol and how confident that is.

use regex::Regex;

use crate::rank::is_comment_line;

/// A line recognised as a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definition {
    /// Short kind label (`fn`, `type`, `macro`, `impl`, `method`, ...).
    pub kind: &'static str,
    /// Confidence / importance score (higher is better).
    pub score: i32,
}

/// Language families with their own definition syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Go,
    C,
    JavaLike,
    JsTs,
    Python,
    Ruby,
    Php,
    Shell,
    Lua,
    Elixir,
    Zig,
    Docs,
    Generic,
}

/// Map a path to its language family.
pub fn lang_of(path: &str) -> Lang {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => Lang::Rust,
        "go" => Lang::Go,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "m" | "mm" => Lang::C,
        "java" | "cs" | "kt" | "kts" | "scala" | "sc" | "swift" | "dart" | "groovy" => {
            Lang::JavaLike
        }
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte" => {
            Lang::JsTs
        }
        "py" | "pyi" => Lang::Python,
        "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "sh" | "bash" | "zsh" => Lang::Shell,
        "lua" => Lang::Lua,
        "ex" | "exs" => Lang::Elixir,
        "zig" => Lang::Zig,
        "md" | "markdown" | "rst" | "txt" | "adoc" | "org" => Lang::Docs,
        _ => Lang::Generic,
    }
}

/// Pluggable definition detector.
pub trait DefinitionBackend {
    /// Backend name shown in tool output.
    fn name(&self) -> &'static str;
    /// Regex used to fetch candidate lines from the index.
    fn prefilter(&self, symbol: &str) -> String;
    /// Compile per-symbol matchers. `case_insensitive` is used for "did you mean" retries.
    fn compile(&self, symbol: &str, case_insensitive: bool) -> CompiledDefs;
}

/// Regex-heuristic backend (default).
pub struct HeuristicBackend;

impl DefinitionBackend for HeuristicBackend {
    fn name(&self) -> &'static str {
        "regex-heuristic"
    }

    fn prefilter(&self, symbol: &str) -> String {
        format!(r"\b{}\b", regex::escape(symbol))
    }

    fn compile(&self, symbol: &str, case_insensitive: bool) -> CompiledDefs {
        CompiledDefs::new(symbol, case_insensitive)
    }
}

struct Rule {
    re: Regex,
    kind: &'static str,
    score: i32,
}

/// Matchers compiled for one symbol.
pub struct CompiledDefs {
    rules: Vec<(Lang, Vec<Rule>)>,
    /// `type name(` detector for C-like and Java-like languages.
    c_fn: Option<Regex>,
}

const MODS_RUST: &str =
    r#"(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:default|async|const|unsafe|extern(?:\s+"[^"]*")?)\s+)*"#;
const MODS_JAVA: &str = r"(?:@\w+(?:\([^)]*\))?\s+)*(?:(?:public|private|protected|internal|static|final|abstract|sealed|open|override|data|inline|suspend|partial|readonly|async|virtual|extern|unsafe|synchronized|native|default|strictfp|new|lateinit|private\s+set|fileprivate|mutating|class)\s+)*";
const MODS_JS: &str = r"(?:(?:export|default|declare|public|private|protected|static|async|readonly|override|abstract|get|set|accessor)\s+)*";

/// Leading words that turn a `name(` line into a statement, not a definition.
const NOT_TYPE_WORDS: &[&str] = &[
    "return", "else", "if", "while", "for", "switch", "case", "do", "goto", "sizeof", "new",
    "delete", "throw", "await", "yield", "typeof", "not", "and", "or", "in", "is", "echo", "print",
];

impl CompiledDefs {
    fn new(symbol: &str, ci: bool) -> Self {
        let s = regex::escape(symbol);
        let flag = if ci { "(?i)" } else { "" };
        let mk = |src: String, kind: &'static str, score: i32| -> Option<Rule> {
            Regex::new(&format!("{}{}", flag, src))
                .ok()
                .map(|re| Rule { re, kind, score })
        };
        let build = |v: Vec<Option<Rule>>| -> Vec<Rule> { v.into_iter().flatten().collect() };

        let rust = build(vec![
            mk(format!(r"^\s*{MODS_RUST}fn\s+{s}\b"), "fn", 100),
            mk(
                format!(r"^\s*{MODS_RUST}(?:struct|enum|union|trait|type)\s+{s}\b"),
                "type",
                100,
            ),
            mk(format!(r"^\s*{MODS_RUST}mod\s+{s}\b"), "mod", 50),
            mk(
                format!(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:const|static(?:\s+mut)?)\s+{s}\s*:"),
                "const",
                80,
            ),
            mk(format!(r"^\s*macro_rules!\s+{s}\b"), "macro", 100),
            // impl<T> Trait for Sym / impl Sym
            mk(
                format!(r"^\s*(?:unsafe\s+)?impl\b.*\bfor\s+(?:\w+::)*{s}\b"),
                "impl",
                60,
            ),
            mk(
                format!(
                    r"^\s*(?:unsafe\s+)?impl(?:\s*<.*>)?\s+(?:\w+::)*{s}\b(?:\s*<.*>)?\s*(?:\{{|where\b|$)"
                ),
                "impl",
                60,
            ),
            // impl Sym for X (Sym is the trait)
            mk(
                format!(r"^\s*(?:unsafe\s+)?impl(?:\s*<.*?>)?\s+(?:\w+::)*{s}\b(?:<.*?>)?\s+for\b"),
                "trait impl",
                40,
            ),
            // enum variant / struct field are too noisy; skipped on purpose.
        ]);

        let go = build(vec![
            mk(
                format!(r"^func\s+(?:\([^)]*\)\s*)?{s}\s*[\[(]"),
                "func",
                100,
            ),
            mk(format!(r"^\s*type\s+{s}\b"), "type", 100),
            mk(format!(r"^(?:var|const)\s+{s}\b"), "var", 70),
            // Inside `const ( ... )` / `var ( ... )` blocks: `\tName = ...` / `\tName Type = ...`.
            mk(format!(r"^\t{s}(?:\s+[\w.\[\]*]+)?\s*=[^=]"), "const", 40),
        ]);

        let c = build(vec![
            mk(format!(r"^\s*#\s*define\s+{s}\b"), "macro", 100),
            mk(
                format!(
                    r"^\s*(?:typedef\s+)?(?:struct|union|enum|class)(?:\s+\w+)*?\s+{s}\s*(?:[:{{]|$)"
                ),
                "type",
                100,
            ),
            mk(format!(r"^\}}\s*{s}\s*;"), "typedef", 90),
            mk(format!(r"^\s*typedef\b.*\b{s}\s*;"), "typedef", 90),
            mk(format!(r"^\s*using\s+{s}\s*="), "typedef", 90),
            mk(format!(r"^\s*namespace\s+{s}\b"), "namespace", 70),
        ]);
        // C-style function: `type name(` (checked separately, see classify_c_fn).

        let java = build(vec![
            mk(
                format!(
                    r"^\s*{MODS_JAVA}(?:class|interface|enum|record|struct|object|trait|protocol|extension|typealias|actor|@interface)\s+{s}\b"
                ),
                "type",
                100,
            ),
            mk(
                format!(r"^\s*{MODS_JAVA}(?:fun|func|def)\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?{s}\b"),
                "fn",
                100,
            ),
            mk(
                format!(r"^\s*{MODS_JAVA}(?:val|var|let)\s+{s}\b"),
                "var",
                50,
            ),
        ]);

        let js = build(vec![
            mk(
                format!(r"^\s*{MODS_JS}function\s*\*?\s*{s}\b"),
                "function",
                100,
            ),
            mk(
                format!(
                    r"^\s*{MODS_JS}(?:const\s+)?(?:class|interface|enum|namespace|module)\s+{s}\b"
                ),
                "type",
                100,
            ),
            mk(format!(r"^\s*{MODS_JS}type\s+{s}\b\s*[=<]"), "type", 100),
            // Top-level (column 0) const/let/var; indented ones are locals.
            mk(
                format!(r"^(?:export\s+)?(?:declare\s+)?(?:const|let|var)\s+{s}\b"),
                "const",
                90,
            ),
            // Class / object method: `  name(args) {` or `  async name<T>(...): T {`
            mk(
                format!(
                    r"^\s+{MODS_JS}\*?{s}\s*(?:<[^>]*>)?\s*\([^;]*\)\s*(?::\s*[^=;{{]+)?\s*\{{"
                ),
                "method",
                90,
            ),
            // One-line class: `class Foo { name() { ... } }`
            mk(
                format!(r"\bclass\s+\w+[^{{]*\{{\s*{MODS_JS}\*?{s}\s*\("),
                "method",
                80,
            ),
            mk(
                format!(r"^\s*{s}\s*:\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*=>|\w+\s*=>)"),
                "method",
                80,
            ),
            mk(
                format!(r"^\s*(?:this|exports|module\.exports|\w+\.prototype)\.{s}\s*=[^=]"),
                "assign",
                70,
            ),
        ]);

        let python = build(vec![
            mk(format!(r"^\s*(?:async\s+)?def\s+{s}\b"), "def", 100),
            mk(format!(r"^\s*class\s+{s}\b"), "class", 100),
            mk(format!(r"^{s}\s*(?::[^=]+)?=[^=]"), "assign", 60),
        ]);

        let ruby = build(vec![
            mk(format!(r"^\s*def\s+(?:self\.)?{s}\b"), "def", 100),
            mk(
                format!(r"^\s*(?:class|module)\s+(?:\w+::)*{s}\b"),
                "class",
                100,
            ),
        ]);

        let php = build(vec![
            mk(
                format!(
                    r"^\s*(?:(?:public|private|protected|static|abstract|final)\s+)*function\s+&?{s}\b"
                ),
                "function",
                100,
            ),
            mk(
                format!(r"^\s*(?:abstract\s+|final\s+)?(?:class|interface|trait|enum)\s+{s}\b"),
                "type",
                100,
            ),
        ]);

        let shell = build(vec![
            mk(format!(r"^\s*function\s+{s}\b"), "function", 100),
            mk(format!(r"^\s*{s}\s*\(\s*\)"), "function", 100),
        ]);

        let lua = build(vec![mk(
            format!(r"^\s*(?:local\s+)?function\s+(?:[\w.:]+[.:])?{s}\b"),
            "function",
            100,
        )]);

        let elixir = build(vec![
            mk(
                format!(r"^\s*(?:def|defp|defmacro|defmacrop|defguard)\s+{s}\b"),
                "def",
                100,
            ),
            mk(
                format!(r"^\s*defmodule\s+(?:[\w.]+\.)?{s}\b"),
                "module",
                100,
            ),
        ]);

        let zig = build(vec![
            mk(
                format!(r"^\s*(?:pub\s+)?(?:inline\s+)?fn\s+{s}\b"),
                "fn",
                100,
            ),
            mk(format!(r"^\s*(?:pub\s+)?const\s+{s}\b\s*="), "const", 90),
        ]);

        let generic = build(vec![mk(
            format!(
                r"^\s*(?:(?:pub|export|public|private|static)\s+)*(?:fn|struct|enum|trait|type|impl|class|def|function|func|fun|interface|module|object)\s+{s}\b"
            ),
            "def",
            80,
        )]);

        let rules = vec![
            (Lang::Rust, rust),
            (Lang::Go, go),
            (Lang::C, c),
            (Lang::JavaLike, java),
            (Lang::JsTs, js),
            (Lang::Python, python),
            (Lang::Ruby, ruby),
            (Lang::Php, php),
            (Lang::Shell, shell),
            (Lang::Lua, lua),
            (Lang::Elixir, elixir),
            (Lang::Zig, zig),
            (Lang::Generic, generic),
        ];
        let c_fn = Regex::new(&format!(
            r"{flag}^(\s*)((?:[A-Za-z_][\w:<>,\*&\[\]]*[\s\*&]+)*?)(?:\w+::)*~?{s}\s*\("
        ))
        .ok();
        Self { rules, c_fn }
    }

    /// Classify one line of `path`. Returns None for non-definitions and for
    /// lines inside comments.
    pub fn classify(&self, path: &str, line: &str) -> Option<Definition> {
        let lang = lang_of(path);
        if lang == Lang::Docs || is_comment_line(path, line) {
            return None;
        }
        let rules = self
            .rules
            .iter()
            .find(|(l, _)| *l == lang)
            .map(|(_, r)| r.as_slice())
            .unwrap_or(&[]);
        let mut best: Option<Definition> = None;
        for r in rules {
            if r.re.is_match(line) && best.is_none_or(|b| r.score > b.score) {
                best = Some(Definition {
                    kind: r.kind,
                    score: r.score,
                });
            }
        }
        if lang == Lang::C || lang == Lang::JavaLike {
            if let Some(d) = self.classify_c_fn(line, lang) {
                if best.is_none_or(|b| d.score > b.score) {
                    best = Some(d);
                }
            }
        }
        best
    }

    /// `type name(` for C, C++, Java, C# (return type before the name).
    fn classify_c_fn(&self, line: &str, lang: Lang) -> Option<Definition> {
        let re = self.c_fn.as_ref()?;
        let caps = re.captures(line)?;
        let indent = caps.get(1).map_or("", |m| m.as_str());
        let prefix = caps.get(2).map_or("", |m| m.as_str()).trim();
        let first = prefix.split_whitespace().next().unwrap_or("");
        if NOT_TYPE_WORDS.contains(&first) {
            return None;
        }
        let trimmed = line.trim_end();
        let is_decl = trimmed.ends_with(';');
        if prefix.is_empty() {
            // `name(...)` with no return type: only a C++ constructor/K&R style
            // definition at column 0 counts; indented ones are calls.
            if !indent.is_empty() || is_decl || lang == Lang::JavaLike {
                return None;
            }
            return Some(Definition {
                kind: "fn",
                score: 70,
            });
        }
        if is_decl {
            return Some(Definition {
                kind: "declaration",
                score: 50,
            });
        }
        Some(Definition {
            kind: "fn",
            score: 100,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(path: &str, sym: &str, line: &str) -> Option<Definition> {
        HeuristicBackend.compile(sym, false).classify(path, line)
    }

    #[test]
    fn rust_rules() {
        assert!(def("a.rs", "compute", "pub(crate) fn compute() {}").is_some());
        assert!(def("a.rs", "fetch", "pub async fn fetch() {}").is_some());
        assert!(def("a.rs", "danger", "pub unsafe fn danger() {}").is_some());
        assert!(def("a.rs", "cfn", "const fn cfn() {}").is_some());
        assert!(def("a.rs", "my_macro", "macro_rules! my_macro { () => {} }").is_some());
        assert_eq!(
            def("a.rs", "Wrapper", "impl<T> Display for Wrapper<T> {}")
                .unwrap()
                .kind,
            "impl"
        );
        assert_eq!(
            def("a.rs", "Display", "impl<T> Display for Wrapper<T> {}")
                .unwrap()
                .kind,
            "trait impl"
        );
        assert_eq!(def("a.rs", "Foo", "impl Foo {").unwrap().kind, "impl");
        assert!(def("a.rs", "compute", "    let compute = 3;").is_none());
        assert!(def("a.rs", "compute", "/// fn compute() example").is_none());
        assert!(def("a.rs", "compute", "    compute();").is_none());
        assert!(def("a.rs", "MAX", "pub const MAX: usize = 3;").is_some());
    }

    #[test]
    fn go_rules() {
        assert!(def(
            "x.go",
            "HandleLogin",
            "func (s *Server) HandleLogin(w http.ResponseWriter) {"
        )
        .is_some());
        assert!(def("x.go", "Map", "func Map[T any](xs []T) {").is_some());
        assert!(def("x.go", "Server", "type Server struct{}").is_some());
        assert!(def("x.go", "Server", "\ts := Server{}").is_none());
    }

    #[test]
    fn c_rules() {
        assert!(def("x.c", "compute", "static int compute(int a) {").is_some());
        assert!(def("x.c", "MAXV", "#define MAXV 10").is_some());
        assert!(def("x.c", "compute", "  return compute(a);").is_none());
        assert!(def("x.c", "compute", "  x = compute(a);").is_none());
        assert!(def("x.c", "compute", "  compute(a);").is_none());
        assert_eq!(
            def("x.h", "compute", "int compute(int a);").unwrap().kind,
            "declaration"
        );
        assert!(def("x.c", "node", "struct node {").is_some());
        assert!(def("x.c", "node_t", "} node_t;").is_some());
        assert!(def("x.c", "compute", "// int compute(int a) {").is_none());
    }

    #[test]
    fn java_rules() {
        assert!(def(
            "x.java",
            "handleLogin",
            "  public void handleLogin(String u) {}"
        )
        .is_some());
        assert!(def(
            "x.java",
            "compute",
            "  private static int compute(int a) { return a; }"
        )
        .is_some());
        assert!(def("x.java", "Svc", "public class Svc {").is_some());
        assert!(def("x.java", "compute", "    return compute(a);").is_none());
        assert!(def("x.java", "compute", "    int x = compute(a);").is_none());
        assert!(def("x.kt", "load", "    suspend fun load() {").is_some());
    }

    #[test]
    fn js_rules() {
        assert!(def(
            "x.ts",
            "handleLogin",
            "export const handleLogin = async () => {};"
        )
        .is_some());
        assert!(def("x.ts", "App", "export default function App() {}").is_some());
        assert!(def(
            "x.ts",
            "handleLogin",
            "export class Foo { handleLogin() { return 1 } }"
        )
        .is_some());
        assert!(def(
            "x.ts",
            "handleLogin",
            "  async handleLogin(req: Req): Promise<void> {"
        )
        .is_some());
        assert!(def("x.ts", "handleLogin", "    handleLogin(req);").is_none());
        assert!(def("x.ts", "handleLogin", "    const handleLogin = 1;").is_none());
        assert!(def("x.ts", "Props", "export interface Props {").is_some());
        assert!(def("x.ts", "Id", "type Id = string;").is_some());
        assert!(def("x.ts", "handleLogin", "    if (handleLogin(x)) {").is_none());
    }

    #[test]
    fn python_rules() {
        assert!(def(
            "x.py",
            "handleLogin",
            "    async def handleLogin(self, req):"
        )
        .is_some());
        assert!(def("x.py", "handleLogin", "def  handleLogin():").is_some());
        assert!(def("x.py", "Server", "class Server:").is_some());
        assert!(def("x.py", "handleLogin", "# def handleLogin():").is_none());
    }

    #[test]
    fn docs_are_ignored() {
        assert!(def("doc.md", "compute", "fn compute() {}").is_none());
    }

    #[test]
    fn case_insensitive_compile() {
        let c = HeuristicBackend.compile("handlelogin", true);
        assert!(c
            .classify("x.go", "func (s *Server) HandleLogin(w int) {")
            .is_some());
    }
}
