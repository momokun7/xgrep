use regex_syntax::hir::{Class, Hir, HirKind, Literal, Repetition};
use regex_syntax::ParserBuilder;
use std::collections::BTreeSet;

/// Boolean trigram query tree
#[derive(Debug, Clone)]
pub enum TrigramQuery {
    /// Matches all files (no filtering)
    All,
    /// Matches no files
    None,
    /// All sub-queries must match (intersection)
    And(Vec<TrigramQuery>),
    /// Any sub-query must match (union)
    Or(Vec<TrigramQuery>),
    /// Single trigram lookup
    Trigram([u8; 3]),
}

impl TrigramQuery {
    /// Returns true if this query matches all files (no filtering)
    pub fn is_all(&self) -> bool {
        matches!(self, TrigramQuery::All)
    }

    /// Simplify the query tree
    pub fn simplify(self) -> TrigramQuery {
        match self {
            TrigramQuery::And(qs) => {
                let mut simplified: Vec<TrigramQuery> = Vec::new();
                for q in qs {
                    let q = q.simplify();
                    match q {
                        TrigramQuery::All => {}                               // skip All in And
                        TrigramQuery::None => return TrigramQuery::None, // And with None = None
                        TrigramQuery::And(inner) => simplified.extend(inner), // flatten
                        other => simplified.push(other),
                    }
                }
                match simplified.len() {
                    0 => TrigramQuery::All,
                    1 => simplified.into_iter().next().unwrap(),
                    _ => TrigramQuery::And(simplified),
                }
            }
            TrigramQuery::Or(qs) => {
                let mut simplified: Vec<TrigramQuery> = Vec::new();
                for q in qs {
                    let q = q.simplify();
                    match q {
                        TrigramQuery::All => return TrigramQuery::All, // Or with All = All
                        TrigramQuery::None => {}                       // skip None in Or
                        TrigramQuery::Or(inner) => simplified.extend(inner), // flatten
                        other => simplified.push(other),
                    }
                }
                match simplified.len() {
                    0 => TrigramQuery::None,
                    1 => simplified.into_iter().next().unwrap(),
                    _ => TrigramQuery::Or(simplified),
                }
            }
            other => other,
        }
    }

    /// Evaluate query against index reader, return candidate file IDs
    pub fn evaluate(&self, reader: &crate::index::reader::IndexReader) -> Vec<u32> {
        match self {
            TrigramQuery::All => (0..reader.file_count()).collect(),
            TrigramQuery::None => vec![],
            TrigramQuery::Trigram(t) => reader.lookup_trigram(*t),
            TrigramQuery::And(qs) => {
                let mut lists: Vec<Vec<u32>> = Vec::with_capacity(qs.len());
                for q in qs {
                    let list = q.evaluate(reader);
                    if list.is_empty() {
                        return vec![];
                    }
                    lists.push(list);
                }
                if lists.is_empty() {
                    return (0..reader.file_count()).collect();
                }
                let refs: Vec<&[u32]> = lists.iter().map(|v| v.as_slice()).collect();
                crate::candidates::intersect_postings(&refs)
            }
            TrigramQuery::Or(qs) => {
                let mut combined: Vec<u32> = Vec::new();
                for q in qs {
                    combined.extend(q.evaluate(reader));
                }
                combined.sort_unstable();
                combined.dedup();
                combined
            }
        }
    }
}

/// Convert a regex pattern string to a TrigramQuery
#[cfg(test)]
pub fn regex_to_query(pattern: &str) -> TrigramQuery {
    regex_to_query_with(pattern, false)
}

/// Convert a regex pattern to a TrigramQuery, optionally case-insensitive.
///
/// Case folding is resolved by the regex parser itself (the same Unicode
/// simple case folding the matcher uses), so `(?i)`, `[Hh]` and the
/// `case_insensitive` flag all expand into case-variant trigrams.
pub fn regex_to_query_with(pattern: &str, case_insensitive: bool) -> TrigramQuery {
    let hir = match ParserBuilder::new()
        .utf8(false)
        .case_insensitive(case_insensitive)
        .build()
        .parse(pattern)
    {
        Ok(h) => h,
        Err(_) => return TrigramQuery::All, // invalid regex = no filtering
    };
    hir_info(&hir).into_query().simplify()
}

// ---------------------------------------------------------------------------
// Regex analysis (after Russ Cox, "Regular Expression Matching with a Trigram
// Index"). Every rule must be sound: a file containing a match must satisfy
// the produced query.
// ---------------------------------------------------------------------------

/// Max size of an exact string set before it is folded into a query.
const MAX_EXACT: usize = 64;
/// Max size of a prefix/suffix set before it is shortened.
const MAX_AFFIX: usize = 32;
/// Max number of characters a class may expand to.
const MAX_CLASS: usize = 16;
/// Prefix/suffix strings are kept to this many bytes; a trigram spanning a
/// concatenation boundary needs at most 2 bytes from each side.
const AFFIX_LEN: usize = 2;

type ByteSet = BTreeSet<Vec<u8>>;

/// What is known about the strings a regex node can match.
struct Info {
    /// Some(set): the node matches exactly one of these strings.
    exact: Option<ByteSet>,
    /// Every match starts with one of these (meaningful when `exact` is None).
    /// An empty string in the set means "unknown".
    prefix: ByteSet,
    /// Every match ends with one of these (same convention as `prefix`).
    suffix: ByteSet,
    /// Query every match must satisfy (beyond what `exact` implies).
    query: TrigramQuery,
}

impl Info {
    fn exact(set: ByteSet) -> Self {
        Info {
            exact: Some(set),
            prefix: ByteSet::new(),
            suffix: ByteSet::new(),
            query: TrigramQuery::All,
        }
    }

    fn unknown() -> Self {
        let empty: ByteSet = std::iter::once(Vec::new()).collect();
        Info {
            exact: None,
            prefix: empty.clone(),
            suffix: empty,
            query: TrigramQuery::All,
        }
    }

    /// Fold an exact set into query/prefix/suffix form.
    fn make_inexact(&mut self) {
        if let Some(set) = self.exact.take() {
            self.query = set_query(&set);
            self.prefix = shorten_prefixes(set.iter().cloned().collect());
            self.suffix = shorten_suffixes(set.into_iter().collect());
        }
    }

    fn into_query(mut self) -> TrigramQuery {
        self.make_inexact();
        self.query
    }
}

fn hir_info(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::exact(single(Vec::new())),
        HirKind::Literal(Literal(bytes)) => Info::exact(single(bytes.to_vec())),
        HirKind::Class(class) => match expand_class(class) {
            Some(set) => Info::exact(set),
            None => Info::unknown(),
        },
        HirKind::Capture(cap) => hir_info(&cap.sub),
        HirKind::Repetition(Repetition { min, max, sub, .. }) => {
            if *min == 0 {
                let mut info = hir_info(sub);
                match (max, info.exact.as_mut()) {
                    (Some(1), Some(set)) if set.len() < MAX_EXACT => {
                        set.insert(Vec::new());
                        info
                    }
                    _ => Info::unknown(),
                }
            } else {
                // x{n,} matches start with x and end with x.
                let mut info = hir_info(sub);
                info.make_inexact();
                info
            }
        }
        HirKind::Concat(subs) => subs
            .iter()
            .fold(Info::exact(single(Vec::new())), |acc, sub| {
                concat(acc, hir_info(sub))
            }),
        HirKind::Alternation(subs) => {
            let mut infos = subs.iter().map(hir_info);
            let first = infos.next().unwrap_or_else(Info::unknown);
            infos.fold(first, alternate)
        }
    }
}

fn concat(mut x: Info, mut y: Info) -> Info {
    if let (Some(ex), Some(ey)) = (&x.exact, &y.exact) {
        if ex.len() * ey.len() <= MAX_EXACT {
            return Info::exact(cross(ex, ey));
        }
    }
    let x_exact = x.exact.clone();
    let y_exact = y.exact.clone();
    x.make_inexact();
    y.make_inexact();

    let boundary = cross_boundary_trigrams(&x.suffix, &y.prefix);
    let prefix = match &x_exact {
        Some(ex) => shorten_prefixes(cross(ex, &y.prefix)),
        None => x.prefix,
    };
    let suffix = match &y_exact {
        Some(ey) => shorten_suffixes(cross(&x.suffix, ey)),
        None => y.suffix,
    };
    Info {
        exact: None,
        prefix,
        suffix,
        query: TrigramQuery::And(vec![x.query, y.query, boundary]),
    }
}

fn alternate(mut x: Info, mut y: Info) -> Info {
    if let (Some(ex), Some(ey)) = (&mut x.exact, &y.exact) {
        if ex.len() + ey.len() <= MAX_EXACT {
            ex.extend(ey.iter().cloned());
            return x;
        }
    }
    x.make_inexact();
    y.make_inexact();
    // A branch with unknown prefix/suffix contributes "" to the union, which
    // keeps the whole alternation's prefix/suffix unknown.
    x.prefix.extend(y.prefix);
    x.suffix.extend(y.suffix);
    Info {
        exact: None,
        prefix: shorten_prefixes(x.prefix),
        suffix: shorten_suffixes(x.suffix),
        query: TrigramQuery::Or(vec![x.query, y.query]),
    }
}

fn single(s: Vec<u8>) -> ByteSet {
    std::iter::once(s).collect()
}

fn cross(a: &ByteSet, b: &ByteSet) -> ByteSet {
    let mut out = ByteSet::new();
    for x in a {
        for y in b {
            let mut s = Vec::with_capacity(x.len() + y.len());
            s.extend_from_slice(x);
            s.extend_from_slice(y);
            out.insert(s);
        }
    }
    out
}

/// Keep the first AFFIX_LEN bytes of each prefix, shortening further while
/// the set is too large. A prefix of a prefix is still a valid prefix.
fn shorten_prefixes(set: ByteSet) -> ByteSet {
    let mut len = AFFIX_LEN;
    loop {
        let out: ByteSet = set.iter().map(|s| s[..s.len().min(len)].to_vec()).collect();
        if out.len() <= MAX_AFFIX || len == 0 {
            return out;
        }
        len -= 1;
    }
}

/// Suffix counterpart of `shorten_prefixes`.
fn shorten_suffixes(set: ByteSet) -> ByteSet {
    let mut len = AFFIX_LEN;
    loop {
        let out: ByteSet = set
            .iter()
            .map(|s| s[s.len().saturating_sub(len)..].to_vec())
            .collect();
        if out.len() <= MAX_AFFIX || len == 0 {
            return out;
        }
        len -= 1;
    }
}

/// Query satisfied by any file containing one of the strings in `set`.
fn set_query(set: &ByteSet) -> TrigramQuery {
    if set.is_empty() {
        return TrigramQuery::None;
    }
    if set.iter().any(|s| s.len() < 3) {
        return TrigramQuery::All;
    }
    let len = set.iter().next().map_or(0, |s| s.len());
    if set.len() > 8 && set.iter().all(|s| s.len() == len) {
        // Many same-length strings (typically case variants): one OR per
        // position keeps the query small. Weaker than OR-of-ANDs but sound.
        let positions = (0..=len - 3)
            .map(|i| {
                let tris: BTreeSet<[u8; 3]> =
                    set.iter().map(|s| [s[i], s[i + 1], s[i + 2]]).collect();
                TrigramQuery::Or(tris.into_iter().map(TrigramQuery::Trigram).collect())
            })
            .collect();
        return TrigramQuery::And(positions);
    }
    TrigramQuery::Or(set.iter().map(|s| trigrams_from_bytes(s)).collect())
}

/// Generate trigrams from a byte slice
fn trigrams_from_bytes(bytes: &[u8]) -> TrigramQuery {
    if bytes.len() < 3 {
        return TrigramQuery::All;
    }
    let trigrams: Vec<TrigramQuery> = bytes
        .windows(3)
        .map(|w| TrigramQuery::Trigram([w[0], w[1], w[2]]))
        .collect();
    TrigramQuery::And(trigrams)
}

/// Trigrams spanning the junction of a suffix set and a prefix set.
///
/// Any match contains `suf + pre` for some pair, so the result is an OR over
/// pairs. If any pair is shorter than 3 bytes (including unknown, i.e. empty,
/// affixes) nothing can be required and the result is All.
fn cross_boundary_trigrams(suffixes: &ByteSet, prefixes: &ByteSet) -> TrigramQuery {
    let mut groups: BTreeSet<Vec<[u8; 3]>> = BTreeSet::new();
    for suf in suffixes {
        for pre in prefixes {
            let suf_tail = &suf[suf.len().saturating_sub(AFFIX_LEN)..];
            let pre_head = &pre[..pre.len().min(AFFIX_LEN)];
            let mut combined = Vec::with_capacity(4);
            combined.extend_from_slice(suf_tail);
            combined.extend_from_slice(pre_head);
            if combined.len() < 3 {
                return TrigramQuery::All;
            }
            groups.insert(combined.windows(3).map(|w| [w[0], w[1], w[2]]).collect());
        }
    }
    if groups.is_empty() {
        return TrigramQuery::All;
    }
    TrigramQuery::Or(
        groups
            .into_iter()
            .map(|g| TrigramQuery::And(g.into_iter().map(TrigramQuery::Trigram).collect()))
            .collect(),
    )
}

/// Expand a character class into the set of encoded strings it matches.
/// Returns None if it has more than MAX_CLASS members.
fn expand_class(class: &Class) -> Option<ByteSet> {
    let mut out = ByteSet::new();
    match class {
        Class::Unicode(uc) => {
            for range in uc.ranges() {
                for c in range.start()..=range.end() {
                    if out.len() >= MAX_CLASS {
                        return None;
                    }
                    let mut buf = [0u8; 4];
                    out.insert(c.encode_utf8(&mut buf).as_bytes().to_vec());
                }
            }
        }
        Class::Bytes(bc) => {
            for range in bc.ranges() {
                for b in range.start()..=range.end() {
                    if out.len() >= MAX_CLASS {
                        return None;
                    }
                    out.insert(vec![b]);
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_literal_pattern() {
        let q = regex_to_query("hello");
        // Should produce AND(Tri("hel"), Tri("ell"), Tri("llo"))
        match q {
            TrigramQuery::And(ref qs) => {
                assert_eq!(qs.len(), 3);
            }
            _ => panic!("expected And, got {:?}", q),
        }
    }

    #[test]
    fn test_alternation() {
        let q = regex_to_query("foo|bar");
        // Should produce OR(AND(Tri("foo")), AND(Tri("bar")))
        match q {
            TrigramQuery::Or(ref qs) => {
                assert_eq!(qs.len(), 2);
            }
            _ => panic!("expected Or, got {:?}", q),
        }
    }

    #[test]
    fn test_concat_with_wildcard() {
        let q = regex_to_query("foo.*bar");
        // Should produce AND(Tri("foo"), Tri("bar")) -- not just Tri("foo")
        match q {
            TrigramQuery::And(ref qs) => {
                // Should contain trigrams from both "foo" and "bar"
                let has_foo = qs
                    .iter()
                    .any(|q| matches!(q, TrigramQuery::Trigram(t) if t == b"foo"));
                let has_bar = qs
                    .iter()
                    .any(|q| matches!(q, TrigramQuery::Trigram(t) if t == b"bar"));
                assert!(has_foo, "should contain foo trigram");
                assert!(has_bar, "should contain bar trigram");
            }
            _ => panic!("expected And, got {:?}", q),
        }
    }

    #[test]
    fn test_short_pattern() {
        let q = regex_to_query("ab");
        // Too short for trigrams
        assert!(matches!(q, TrigramQuery::All));
    }

    #[test]
    fn test_dot_star() {
        let q = regex_to_query(".*");
        assert!(matches!(q, TrigramQuery::All));
    }

    #[test]
    fn test_simplify_nested_and() {
        let q = TrigramQuery::And(vec![
            TrigramQuery::And(vec![TrigramQuery::Trigram(*b"abc")]),
            TrigramQuery::All,
            TrigramQuery::Trigram(*b"def"),
        ])
        .simplify();
        match q {
            TrigramQuery::And(qs) => {
                assert_eq!(qs.len(), 2); // All removed, inner And flattened
            }
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn test_simplify_or_with_all() {
        let q =
            TrigramQuery::Or(vec![TrigramQuery::Trigram(*b"abc"), TrigramQuery::All]).simplify();
        assert!(matches!(q, TrigramQuery::All)); // Or with All = All
    }

    #[test]
    fn test_evaluate_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "abcdef").unwrap();
        std::fs::write(root.join("b.txt"), "ghijkl").unwrap();
        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();

        // "abc" AND "bcd" should match only a.txt
        let query = TrigramQuery::And(vec![
            TrigramQuery::Trigram(*b"abc"),
            TrigramQuery::Trigram(*b"bcd"),
        ]);
        let results = query.evaluate(&reader);
        assert!(!results.is_empty());
        // All results should include the file containing "abcdef"
    }

    #[test]
    fn test_evaluate_or_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "abcdef").unwrap();
        std::fs::write(root.join("b.txt"), "ghijkl").unwrap();
        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();

        let query = TrigramQuery::Or(vec![
            TrigramQuery::Trigram(*b"abc"),
            TrigramQuery::Trigram(*b"ghi"),
        ]);
        let results = query.evaluate(&reader);
        assert!(results.len() >= 2); // Both files should match
    }

    #[test]
    fn test_evaluate_all() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();

        let query = TrigramQuery::All;
        let results = query.evaluate(&reader);
        assert_eq!(results.len(), reader.file_count() as usize);
    }

    // -- Soundness: no false negatives -------------------------------------

    /// Evaluate a query against the trigram set of an in-memory string.
    fn eval_mem(q: &TrigramQuery, tris: &std::collections::HashSet<[u8; 3]>) -> bool {
        match q {
            TrigramQuery::All => true,
            TrigramQuery::None => false,
            TrigramQuery::Trigram(t) => tris.contains(t),
            TrigramQuery::And(qs) => qs.iter().all(|q| eval_mem(q, tris)),
            TrigramQuery::Or(qs) => qs.iter().any(|q| eval_mem(q, tris)),
        }
    }

    fn candidate(pattern: &str, ci: bool, content: &[u8]) -> bool {
        let tris = content.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
        eval_mem(&regex_to_query_with(pattern, ci), &tris)
    }

    fn is_match(pattern: &str, ci: bool, content: &[u8]) -> bool {
        regex::bytes::RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .multi_line(true)
            .crlf(true)
            .build()
            .unwrap()
            .is_match(content)
    }

    #[test]
    fn test_no_false_negative_repros() {
        let cases: &[(&str, &str)] = &[
            ("fo(o|)bar", "xx fobar xx"),
            ("fo(o|)bar", "xx foobar xx"),
            ("x(ab|.c)yz", "xQcyz"),
            ("x(ab|.c)yz", "xabyz"),
            ("Vec<u(8|16)>", "let v: Vec<u16> = x;"),
            ("Vec<u(8|16)>", "let v: Vec<u8> = x;"),
            ("ab(c|)de", "abde"),
            ("(a|bc)def", "adef"),
            ("abc(d|e)?fgh", "abcfgh"),
        ];
        for (pat, text) in cases {
            assert!(is_match(pat, false, text.as_bytes()), "{pat} / {text}");
            assert!(
                candidate(pat, false, text.as_bytes()),
                "false negative: {pat} on {text:?}"
            );
        }
    }

    #[test]
    fn test_case_insensitive_regex_uses_index() {
        for pat in ["(?i)hashmap", "[Hh]ash[Mm]ap"] {
            let q = regex_to_query_with(pat, false);
            assert!(!q.is_all(), "{pat} should not be a full scan");
            for text in ["HashMap", "hashmap", "HASHMAP", "hashMap"] {
                if is_match(pat, false, text.as_bytes()) {
                    assert!(candidate(pat, false, text.as_bytes()), "{pat} / {text}");
                }
            }
            assert!(!candidate(pat, false, b"unrelated text"));
        }
        let q = regex_to_query_with("hashmap", true);
        assert!(!q.is_all());
        assert!(candidate("hashmap", true, b"x HashMap y"));
        // Unicode simple case folding: (?i)k also matches KELVIN SIGN.
        assert!(candidate("(?i)kab", false, "\u{212A}ab".as_bytes()));
    }

    #[test]
    fn test_alternation_with_unknown_branch_has_unknown_prefix() {
        // The `.c` branch has an unknown prefix, so no trigram may span
        // the boundary between `x` and the alternation.
        let q = regex_to_query("x(ab|.c)yz");
        assert!(candidate("x(ab|.c)yz", false, b"xQcyz"));
        assert!(!q.is_all());
    }

    /// Generate (regex, sample) pairs where `sample` matches `regex`.
    fn regex_strategy() -> impl Strategy<Value = (String, String)> {
        let leaf = prop_oneof![
            prop::sample::select(vec!['a', 'b', 'c', 'k', 's', 'A', 'B', 'K', 'é', '-'])
                .prop_map(|c| (c.to_string(), c.to_string())),
            prop::sample::select(vec!['a', 'b', 'x', 'é', ' '])
                .prop_map(|c| (".".to_string(), c.to_string())),
            prop::bool::ANY.prop_map(|b| ("[ab]".to_string(), if b { "a" } else { "b" }.into())),
            prop::bool::ANY.prop_map(|b| ("[Kk]".to_string(), if b { "K" } else { "k" }.into())),
            Just((r"\d".to_string(), "7".to_string())),
            Just((r"\b".to_string(), String::new())),
        ];
        leaf.prop_recursive(5, 48, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..5).prop_map(|parts| {
                    let re = parts.iter().map(|(r, _)| format!("(?:{r})")).collect();
                    let s = parts.iter().map(|(_, s)| s.as_str()).collect();
                    (re, s)
                }),
                (inner.clone(), inner.clone(), prop::bool::ANY).prop_map(|(a, b, pick)| {
                    let s = if pick { a.1.clone() } else { b.1.clone() };
                    (format!("(?:{}|{})", a.0, b.0), s)
                }),
                (inner.clone(), prop::bool::ANY).prop_map(|(a, pick)| {
                    let s = if pick { a.1.clone() } else { String::new() };
                    (format!("(?:{}|)", a.0), s)
                }),
                (inner.clone(), prop::bool::ANY).prop_map(|(a, pick)| {
                    let s = if pick { a.1.clone() } else { String::new() };
                    (format!("(?:{})?", a.0), s)
                }),
                (inner.clone(), 0usize..3)
                    .prop_map(|(a, n)| (format!("(?:{})*", a.0), a.1.repeat(n))),
                (inner.clone(), 1usize..3)
                    .prop_map(|(a, n)| (format!("(?:{})+", a.0), a.1.repeat(n))),
                (inner.clone(), any::<u64>()).prop_map(|(a, seed)| {
                    let s =
                        a.1.chars()
                            .enumerate()
                            .map(|(i, c)| match (seed >> (i % 64)) & 1 {
                                1 if c == 'k' => '\u{212A}',
                                1 => c.to_ascii_uppercase(),
                                _ => c.to_ascii_lowercase(),
                            })
                            .collect();
                    (format!("(?i:{})", a.0), s)
                }),
            ]
        })
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2000))]

        /// A file whose content matches the regex must be a candidate.
        #[test]
        fn prop_no_false_negatives(
            (re, sample) in regex_strategy(),
            pre in "[a-cA-C \n]{0,4}",
            post in "[a-cA-C \n]{0,4}",
            ci in proptest::bool::ANY,
        ) {
            let content = format!("{pre}{sample}{post}");
            proptest::prop_assume!(content.len() >= 3);
            proptest::prop_assume!(is_match(&re, ci, content.as_bytes()));
            proptest::prop_assert!(
                candidate(&re, ci, content.as_bytes()),
                "false negative: regex={:?} ci={} content={:?}", re, ci, content
            );
        }

        /// Same invariant with arbitrary text instead of constructed samples.
        #[test]
        fn prop_no_false_negatives_random_text(
            (re, _) in regex_strategy(),
            content in "[abckKsAB\u{212A}é \n-]{3,24}",
            ci in proptest::bool::ANY,
        ) {
            if is_match(&re, ci, content.as_bytes()) {
                proptest::prop_assert!(
                    candidate(&re, ci, content.as_bytes()),
                    "false negative: regex={:?} ci={} content={:?}", re, ci, content
                );
            }
        }
    }

    #[test]
    fn test_index_level_repros() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "fobar\n").unwrap();
        std::fs::write(root.join("b.txt"), "xQcyz\n").unwrap();
        std::fs::write(root.join("c.rs"), "let v: Vec<u16> = x;\n").unwrap();
        std::fs::write(root.join("d.rs"), "use std::collections::HashMap;\n").unwrap();
        std::fs::write(root.join("e.txt"), "nothing relevant here\n").unwrap();
        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
        let files_for = |pat: &str| -> Vec<String> {
            regex_to_query(pat)
                .evaluate(&reader)
                .into_iter()
                .map(|id| reader.file_path(id).to_string())
                .collect()
        };
        assert!(files_for("fo(o|)bar").contains(&"a.txt".to_string()));
        assert!(files_for("x(ab|.c)yz").contains(&"b.txt".to_string()));
        assert!(files_for("Vec<u(8|16)>").contains(&"c.rs".to_string()));
        let hm = files_for("(?i)hashmap");
        assert_eq!(hm, vec!["d.rs".to_string()]);
    }
}
