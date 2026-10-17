//! Candidate file resolution from trigram index queries.

use crate::index::reader::IndexReader;
use crate::trigram::{fold, next_bit};
use crate::trigram_query;

// ---------------------------------------------------------------------------
// Candidate resolution (narrowing down from index)
// ---------------------------------------------------------------------------

/// Max number of distinct trigrams decoded for one literal, rarest first.
///
/// Every additional trigram can only shrink the candidate set, but the gain
/// drops quickly once the set is small while the most common trigrams have
/// the longest posting lists. Skipping some is always sound. On a 13.8K-file
/// Rust corpus, 12 kept the candidates of long literals within ~1-40 % of
/// using every trigram (6 was up to 45 % worse) at about half the index time.
const MAX_LITERAL_TERMS: usize = 12;

/// Resolve candidate file IDs for literal string search.
///
/// The index stores ASCII-case-folded trigrams, so the folded pattern is
/// looked up for both case-sensitive and case-insensitive searches (the
/// content match afterwards decides). For patterns of 3+ bytes the `loc` /
/// `next` masks are checked as well (see [`literal_candidates`]).
pub(crate) fn resolve_literal_candidates(
    reader: &IndexReader,
    _original_pattern: &str,
    search_pattern: &str,
    _trigrams: &[[u8; 3]],
    _case_insensitive: bool,
) -> Vec<u32> {
    let pat: Vec<u8> = search_pattern.bytes().map(fold).collect();
    match pat.len() {
        0 | 1 => (0..reader.file_count()).collect(),
        // Every occurrence of a 2-byte string produces a trigram with that
        // prefix (files end with a virtual sentinel byte), so the prefix
        // range of the sorted trigram table covers all matches.
        2 => reader.lookup_trigram_prefix([pat[0], pat[1]]),
        _ => literal_candidates(reader, &pat),
    }
}

/// One distinct trigram of a literal and where it occurs in the literal.
struct Term<'a> {
    raw: &'a [u8],
    /// (offset in the literal, `next` bit of the following literal byte or 0)
    uses: Vec<(usize, u8)>,
}

impl Term<'_> {
    /// Possible start offsets (mod 8) of the literal in a file whose posting
    /// for this trigram has masks `loc` / `next`; 0 rules the file out.
    ///
    /// Soundness: if the literal starts at byte `s` of the file, the trigram
    /// used at literal offset `o` starts at `s + o`, so bit `(s + o) % 8` is in
    /// `loc` and rotating right by `o` puts it at bit `s % 8`; the byte after
    /// it is the literal's byte `o + 3`, whose bit is therefore in `next`.
    /// Masks are supersets of the true positions/followers, so a true match
    /// always keeps bit `s % 8` set.
    #[inline]
    fn start_mask(&self, loc: u8, next: u8) -> u8 {
        let mut s = 0xFF;
        for &(off, nb) in &self.uses {
            if next & nb != nb {
                return 0;
            }
            s &= loc.rotate_right((off % 8) as u32);
        }
        s
    }
}

/// Candidates for a folded literal of 3+ bytes.
///
/// Query plan: collect the distinct trigrams with their raw posting bytes
/// (any missing trigram means no candidates, before decoding anything),
/// order them by posting byte length (cheapest/rarest first, known from the
/// trigram table), keep the [`MAX_LITERAL_TERMS`] rarest, then intersect
/// lazily: each list is decoded only while candidates remain. Along the way
/// each candidate keeps the set of literal start offsets mod 8 that are
/// consistent with all `loc` / `next` masks seen so far; a candidate whose
/// set becomes empty cannot contain the literal and is dropped.
pub(crate) fn literal_candidates(reader: &IndexReader, pat: &[u8]) -> Vec<u32> {
    debug_assert!(pat.len() >= 3);
    let mut terms: Vec<([u8; 3], Term<'_>)> = Vec::new();
    for i in 0..=pat.len() - 3 {
        let tri = [pat[i], pat[i + 1], pat[i + 2]];
        let nb = pat.get(i + 3).map_or(0, |&b| next_bit(b));
        if let Some((_, t)) = terms.iter_mut().find(|(t, _)| *t == tri) {
            t.uses.push((i, nb));
            continue;
        }
        let Some(raw) = reader.posting_raw(tri) else {
            return Vec::new();
        };
        terms.push((
            tri,
            Term {
                raw,
                uses: vec![(i, nb)],
            },
        ));
    }
    terms.sort_by_key(|(_, t)| t.raw.len());
    terms.truncate(MAX_LITERAL_TERMS);

    let mut cand: Vec<u32> = Vec::new();
    let mut starts: Vec<u8> = Vec::new();
    for (n, (_, term)) in terms.iter().enumerate() {
        let p = IndexReader::decode_posting(term.raw);
        if n == 0 {
            for (i, &id) in p.ids.iter().enumerate() {
                let s = term.start_mask(p.loc(i), p.next(i));
                if s != 0 {
                    cand.push(id);
                    starts.push(s);
                }
            }
        } else {
            let mut k = 0usize;
            let mut j = 0usize;
            for c in 0..cand.len() {
                let id = cand[c];
                while j < p.ids.len() && p.ids[j] < id {
                    j += 1;
                }
                if j < p.ids.len() && p.ids[j] == id {
                    let s = starts[c] & term.start_mask(p.loc(j), p.next(j));
                    if s != 0 {
                        cand[k] = id;
                        starts[k] = s;
                        k += 1;
                    }
                }
            }
            cand.truncate(k);
            starts.truncate(k);
        }
        if cand.is_empty() {
            break;
        }
    }
    cand
}

/// Resolve candidate file IDs for regex search.
pub(crate) fn resolve_regex_candidates(
    reader: &IndexReader,
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
) -> Vec<u32> {
    // Case folding is expanded inside the query (Unicode simple folding,
    // matching the regex engine), so `(?i)` and `[Hh]` still use the index.
    let query = trigram_query::regex_to_query_with(pattern, case_insensitive);
    if query.is_all() && !quiet {
        eprintln!(
            "xgrep: warning: regex '{}' cannot be optimized with trigram index (full scan)",
            pattern
        );
    }
    query.evaluate(reader)
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

pub fn intersect_postings(lists: &[&[u32]]) -> Vec<u32> {
    if lists.is_empty() {
        return vec![];
    }
    if lists.len() == 1 {
        return lists[0].to_vec();
    }

    // Sort by length to start from shortest list
    let mut sorted_lists: Vec<&[u32]> = lists.to_vec();
    sorted_lists.sort_by_key(|l| l.len());

    let mut result: Vec<u32> = sorted_lists[0].to_vec();
    for list in &sorted_lists[1..] {
        let mut j = 0;
        result.retain(|&x| {
            while j < list.len() && list[j] < x {
                j += 1;
            }
            let found = j < list.len() && list[j] == x;
            if found {
                j += 1;
            }
            found
        });
        if result.is_empty() {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intersect_postings() {
        let a = vec![1, 3, 5, 7, 9];
        let b = vec![2, 3, 5, 8];
        let c = vec![3, 5, 6];
        let result = intersect_postings(&[&a, &b, &c]);
        assert_eq!(result, vec![3, 5]);
    }

    #[test]
    fn test_intersect_empty() {
        let a = vec![1u32, 2, 3];
        let result = intersect_postings(&[&a, &[]]);
        assert_eq!(result, Vec::<u32>::new());
    }

    #[test]
    fn test_intersect_single() {
        let a = vec![1, 2, 3];
        let result = intersect_postings(&[&a]);
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn test_intersect_postings_all_same() {
        let a = vec![1, 2, 3, 4, 5];
        let b = vec![1, 2, 3, 4, 5];
        let result = intersect_postings(&[&a, &b]);
        assert_eq!(result, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_intersect_postings_no_overlap() {
        let a = vec![1, 3, 5];
        let b = vec![2, 4, 6];
        let result = intersect_postings(&[&a, &b]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_postings_empty_input() {
        let result = intersect_postings(&[]);
        assert!(result.is_empty());
    }
}

#[cfg(test)]
mod literal_tests {
    use super::*;
    use crate::index::builder;
    use proptest::prelude::*;
    use std::fs;

    fn index_of(files: &[(String, Vec<u8>)]) -> (tempfile::TempDir, IndexReader) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        for (name, content) in files {
            fs::write(root.join(name), content).unwrap();
        }
        let index_path = dir.path().join("index.xgrep");
        builder::build_index(&root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        (dir, reader)
    }

    fn names(reader: &IndexReader, ids: &[u32]) -> Vec<String> {
        let mut v: Vec<String> = ids
            .iter()
            .map(|&i| reader.file_path(i).to_string())
            .collect();
        v.sort();
        v
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
    }

    fn resolve(reader: &IndexReader, pattern: &str, ci: bool) -> Vec<u32> {
        let p = if ci {
            pattern.to_ascii_lowercase()
        } else {
            pattern.to_string()
        };
        resolve_literal_candidates(reader, pattern, &p, &[], ci)
    }

    fn files(list: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
        list.iter()
            .map(|(n, c)| (n.to_string(), c.to_vec()))
            .collect()
    }

    #[test]
    fn test_masks_reject_scattered_trigrams() {
        let (_d, reader) = index_of(&files(&[
            ("hit.txt", b"xx abcd yy"),
            // "abc" and "bcd" both present, never adjacent: loc masks differ.
            ("loc.txt", b"abcX bcd"),
            // "abc" only ever followed by 'z': next mask lacks 'd'.
            ("next.txt", b"abcz zbcd"),
        ]));
        assert_eq!(
            names(&reader, &resolve(&reader, "abcd", false)),
            vec!["hit.txt"]
        );
    }

    #[test]
    fn test_case_sensitive_uses_folded_index() {
        let (_d, reader) = index_of(&files(&[
            ("a.rs", b"impl Drop for X"),
            ("b.rs", b"impl drop for x"),
            ("c.rs", b"nothing"),
        ]));
        // Case-sensitive lookup returns the folded superset; matching decides.
        assert_eq!(
            names(&reader, &resolve(&reader, "impl Drop for", false)),
            vec!["a.rs", "b.rs"]
        );
        assert_eq!(
            names(&reader, &resolve(&reader, "IMPL DROP", true)),
            vec!["a.rs", "b.rs"]
        );
    }

    #[test]
    fn test_two_byte_and_edge_patterns() {
        let (_d, reader) = index_of(&files(&[
            ("end.txt", b"xyz->"),
            ("two.txt", b"->"),
            ("mid.txt", b"a -> b"),
            ("no.txt", b"- >"),
        ]));
        assert_eq!(
            names(&reader, &resolve(&reader, "->", false)),
            vec!["end.txt", "mid.txt", "two.txt"]
        );
        assert_eq!(resolve(&reader, "", false).len(), 4);
        assert_eq!(resolve(&reader, "x", false).len(), 4);
        // Pattern at the very end of a file (no following byte).
        assert_eq!(
            names(&reader, &resolve(&reader, "yz->", false)),
            vec!["end.txt"]
        );
        // Repeated trigram at several offsets ("aaa" at 0 and 1).
        let (_d2, r2) = index_of(&files(&[("a.txt", b"xaaaay"), ("b.txt", b"aaa aaa")]));
        assert_eq!(names(&r2, &resolve(&r2, "aaaa", false)), vec!["a.txt"]);
    }

    /// Files over a small alphabet so that partial matches are common.
    fn file_strategy() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(prop::sample::select(b"abAB c\n_".to_vec()), 0..90)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// Soundness of folding + masks + query planning: every file that
        /// contains the literal (exactly, or ASCII-case-insensitively for
        /// `ci`) must be a candidate.
        #[test]
        fn prop_literal_candidates_are_superset(
            contents in prop::collection::vec(file_strategy(), 1..12),
            pick in any::<prop::sample::Index>(),
            start in any::<prop::sample::Index>(),
            len in 1usize..14,
            random_pat in prop::collection::vec(prop::sample::select(b"abAB c_".to_vec()), 1..8),
            use_random in any::<bool>(),
            ci in any::<bool>(),
        ) {
            let files: Vec<(String, Vec<u8>)> = contents
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("f{i}.txt"), c.clone()))
                .collect();
            // Usually take the pattern from a file so that matches exist.
            let src = &contents[pick.index(contents.len())];
            let pat: Vec<u8> = if use_random || src.is_empty() {
                random_pat
            } else {
                let s = start.index(src.len());
                src[s..(s + len).min(src.len())].to_vec()
            };
            let pattern = String::from_utf8(pat).unwrap();
            let (_d, reader) = index_of(&files);
            let got = names(&reader, &resolve(&reader, &pattern, ci));
            for (name, content) in &files {
                let matched = if ci {
                    contains(&content.to_ascii_lowercase(), pattern.to_ascii_lowercase().as_bytes())
                } else {
                    contains(content, pattern.as_bytes())
                };
                if matched {
                    prop_assert!(
                        got.contains(name),
                        "false negative: pattern={:?} ci={} file={} content={:?}",
                        pattern, ci, name, String::from_utf8_lossy(content)
                    );
                }
            }
        }
    }
}
