use regex_syntax::hir::{Class, Hir, HirKind, Literal, Repetition};
use regex_syntax::Parser;

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
                let mut lists: Vec<Vec<u32>> = qs.iter().map(|q| q.evaluate(reader)).collect();
                if lists.is_empty() {
                    return (0..reader.file_count()).collect();
                }
                // Sort by length (shortest first for efficiency)
                lists.sort_by_key(|l| l.len());
                let mut result = lists[0].clone();
                for list in &lists[1..] {
                    result = intersect_sorted(&result, list);
                    if result.is_empty() {
                        break;
                    }
                }
                result
            }
            TrigramQuery::Or(qs) => {
                let mut combined = std::collections::BTreeSet::new();
                for q in qs {
                    for id in q.evaluate(reader) {
                        combined.insert(id);
                    }
                }
                combined.into_iter().collect()
            }
        }
    }
}

/// Intersect two sorted u32 slices
fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}

/// Convert a regex pattern string to a TrigramQuery
pub fn regex_to_query(pattern: &str) -> TrigramQuery {
    let hir = match Parser::new().parse(pattern) {
        Ok(h) => h,
        Err(_) => return TrigramQuery::All, // invalid regex = no filtering
    };
    let (query, _, _) = hir_to_query(&hir);
    query.simplify()
}

/// Recursively convert Hir AST to TrigramQuery
/// Returns: (query, prefix_bytes, suffix_bytes)
///   prefix: possible byte sequences at the start of this node (for boundary trigrams)
///   suffix: possible byte sequences at the end of this node (for boundary trigrams)
fn hir_to_query(hir: &Hir) -> (TrigramQuery, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    match hir.kind() {
        HirKind::Literal(Literal(bytes)) => {
            let q = trigrams_from_bytes(bytes);
            let pre = vec![bytes.to_vec()];
            let suf = vec![bytes.to_vec()];
            (q, pre, suf)
        }
        HirKind::Concat(subs) => {
            let mut queries = Vec::new();
            let mut prev_suffix: Vec<Vec<u8>> = Vec::new();
            let mut first_prefix: Vec<Vec<u8>> = Vec::new();
            let mut last_suffix: Vec<Vec<u8>>;

            for (i, sub) in subs.iter().enumerate() {
                let (q, pre, suf) = hir_to_query(sub);
                queries.push(q);

                // Generate boundary trigrams between previous suffix and current prefix
                if !prev_suffix.is_empty() && !pre.is_empty() {
                    let boundary = cross_boundary_trigrams(&prev_suffix, &pre);
                    queries.push(boundary);
                }

                if i == 0 {
                    first_prefix = pre;
                }
                last_suffix = suf.clone();
                prev_suffix = suf;
                // Suppress unused variable warning - last_suffix is used after the loop
                let _ = &last_suffix;
            }

            let query = TrigramQuery::And(queries);
            (query, first_prefix, prev_suffix)
        }
        HirKind::Alternation(subs) => {
            let mut queries = Vec::new();
            let mut all_prefixes = Vec::new();
            let mut all_suffixes = Vec::new();

            for sub in subs {
                let (q, pre, suf) = hir_to_query(sub);
                queries.push(q);
                all_prefixes.extend(pre);
                all_suffixes.extend(suf);
            }

            let query = TrigramQuery::Or(queries);
            (query, all_prefixes, all_suffixes)
        }
        HirKind::Repetition(Repetition { min, sub, .. }) => {
            if *min >= 1 {
                let (q, pre, suf) = hir_to_query(sub);
                (q, pre, suf)
            } else {
                // * or ? - might not appear at all
                (TrigramQuery::All, vec![], vec![])
            }
        }
        HirKind::Class(class) => {
            // Expand small character classes into prefix/suffix bytes
            let chars = expand_class(class);
            if let Some(chars) = chars {
                let pre: Vec<Vec<u8>> = chars.iter().map(|&c| vec![c]).collect();
                let suf = pre.clone();
                (TrigramQuery::All, pre, suf)
            } else {
                (TrigramQuery::All, vec![], vec![])
            }
        }
        HirKind::Look(_) => {
            // Lookaround doesn't consume characters
            (TrigramQuery::All, vec![], vec![])
        }
        HirKind::Capture(cap) => hir_to_query(&cap.sub),
        HirKind::Empty => (TrigramQuery::All, vec![vec![]], vec![vec![]]),
    }
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

/// Generate boundary trigrams from suffix bytes and prefix bytes
fn cross_boundary_trigrams(suffixes: &[Vec<u8>], prefixes: &[Vec<u8>]) -> TrigramQuery {
    let mut trigrams = Vec::new();

    for suf in suffixes {
        for pre in prefixes {
            // Combine suffix + prefix and extract trigrams from the junction
            let mut combined = Vec::new();
            // Take last 2 bytes of suffix (at most)
            let suf_start = if suf.len() > 2 { suf.len() - 2 } else { 0 };
            combined.extend_from_slice(&suf[suf_start..]);
            // Take first 2 bytes of prefix (at most)
            let pre_end = pre.len().min(2);
            combined.extend_from_slice(&pre[..pre_end]);

            for w in combined.windows(3) {
                trigrams.push(TrigramQuery::Trigram([w[0], w[1], w[2]]));
            }
        }
    }

    if trigrams.is_empty() {
        TrigramQuery::All
    } else if trigrams.len() == 1 {
        trigrams.into_iter().next().unwrap()
    } else {
        // If multiple suffixes/prefixes, any combination is valid
        TrigramQuery::Or(trigrams)
    }
}

/// Expand a character class into individual bytes if small enough
/// Returns None if too many characters (>16)
fn expand_class(class: &Class) -> Option<Vec<u8>> {
    match class {
        Class::Unicode(uc) => {
            let mut chars = Vec::new();
            for range in uc.ranges() {
                for c in range.start()..=range.end() {
                    if chars.len() > 16 {
                        return None;
                    }
                    if c as u32 <= 127 {
                        chars.push(c as u8);
                    } else {
                        return None; // non-ASCII
                    }
                }
            }
            if chars.is_empty() {
                None
            } else {
                Some(chars)
            }
        }
        Class::Bytes(bc) => {
            let mut bytes = Vec::new();
            for range in bc.ranges() {
                for b in range.start()..=range.end() {
                    if bytes.len() > 16 {
                        return None;
                    }
                    bytes.push(b);
                }
            }
            if bytes.is_empty() {
                None
            } else {
                Some(bytes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
