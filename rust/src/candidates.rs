//! Candidate file resolution from trigram index queries.

use crate::index::reader::IndexReader;
use crate::trigram_query;

// ---------------------------------------------------------------------------
// Candidate resolution (narrowing down from index)
// ---------------------------------------------------------------------------

/// Resolve candidate file IDs for literal string search.
pub(crate) fn resolve_literal_candidates(
    reader: &IndexReader,
    _original_pattern: &str,
    search_pattern: &str,
    trigrams: &[[u8; 3]],
    case_insensitive: bool,
) -> Vec<u32> {
    let pattern_bytes = search_pattern.as_bytes();

    if case_insensitive {
        if trigrams.is_empty() {
            (0..reader.file_count()).collect()
        } else {
            let total_lookups: usize = trigrams.iter().map(|t| case_variants(*t).len()).sum();
            if total_lookups > 64 {
                (0..reader.file_count()).collect()
            } else {
                let mut trigram_candidates: Vec<Vec<u32>> = Vec::new();
                let mut any_empty = false;

                for t in trigrams {
                    let variants = case_variants(*t);
                    let mut union_set = std::collections::BTreeSet::new();
                    for v in &variants {
                        for fid in reader.lookup_trigram(*v) {
                            union_set.insert(fid);
                        }
                    }
                    if union_set.is_empty() {
                        any_empty = true;
                        break;
                    }
                    trigram_candidates.push(union_set.into_iter().collect());
                }

                if any_empty {
                    vec![]
                } else {
                    let refs: Vec<&[u32]> =
                        trigram_candidates.iter().map(|v| v.as_slice()).collect();
                    intersect_postings(&refs)
                }
            }
        }
    } else if trigrams.is_empty() {
        if pattern_bytes.len() == 2 {
            let prefix = [pattern_bytes[0], pattern_bytes[1]];
            let candidates = reader.lookup_trigram_prefix(prefix);
            if candidates.is_empty() {
                (0..reader.file_count()).collect()
            } else {
                candidates
            }
        } else {
            (0..reader.file_count()).collect()
        }
    } else {
        let posting_lists: Vec<Vec<u32>> =
            trigrams.iter().map(|t| reader.lookup_trigram(*t)).collect();
        let refs: Vec<&[u32]> = posting_lists.iter().map(|v| v.as_slice()).collect();
        intersect_postings(&refs)
    }
}

/// Resolve candidate file IDs for regex search.
pub(crate) fn resolve_regex_candidates(
    reader: &IndexReader,
    pattern: &str,
    case_insensitive: bool,
) -> Vec<u32> {
    if case_insensitive {
        let query = trigram_query::regex_to_query(pattern);
        if query.is_all() {
            eprintln!(
                "xgrep: warning: regex '{}' cannot be optimized with trigram index (full scan)",
                pattern
            );
        }
        let query = expand_case_variants_query(query);
        query.evaluate(reader)
    } else {
        let query = trigram_query::regex_to_query(pattern);
        if query.is_all() {
            eprintln!(
                "xgrep: warning: regex '{}' cannot be optimized with trigram index (full scan)",
                pattern
            );
        }
        query.evaluate(reader)
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Expand each Trigram node in a TrigramQuery tree into case variants.
fn expand_case_variants_query(query: trigram_query::TrigramQuery) -> trigram_query::TrigramQuery {
    match query {
        trigram_query::TrigramQuery::Trigram(t) => {
            let variants = case_variants(t);
            if variants.len() == 1 {
                trigram_query::TrigramQuery::Trigram(variants[0])
            } else {
                trigram_query::TrigramQuery::Or(
                    variants
                        .into_iter()
                        .map(trigram_query::TrigramQuery::Trigram)
                        .collect(),
                )
            }
        }
        trigram_query::TrigramQuery::And(qs) => trigram_query::TrigramQuery::And(
            qs.into_iter().map(expand_case_variants_query).collect(),
        ),
        trigram_query::TrigramQuery::Or(qs) => trigram_query::TrigramQuery::Or(
            qs.into_iter().map(expand_case_variants_query).collect(),
        ),
        other => other,
    }
}

/// Generate upper/lowercase variants for each ASCII byte in a trigram.
/// Up to 8 variants (when all 3 bytes are ASCII letters).
pub(crate) fn case_variants(trigram: [u8; 3]) -> Vec<[u8; 3]> {
    let cases: [Vec<u8>; 3] = [
        if trigram[0].is_ascii_alphabetic() {
            vec![
                trigram[0].to_ascii_lowercase(),
                trigram[0].to_ascii_uppercase(),
            ]
        } else {
            vec![trigram[0]]
        },
        if trigram[1].is_ascii_alphabetic() {
            vec![
                trigram[1].to_ascii_lowercase(),
                trigram[1].to_ascii_uppercase(),
            ]
        } else {
            vec![trigram[1]]
        },
        if trigram[2].is_ascii_alphabetic() {
            vec![
                trigram[2].to_ascii_lowercase(),
                trigram[2].to_ascii_uppercase(),
            ]
        } else {
            vec![trigram[2]]
        },
    ];

    let mut variants = Vec::with_capacity(cases[0].len() * cases[1].len() * cases[2].len());
    for &b0 in &cases[0] {
        for &b1 in &cases[1] {
            for &b2 in &cases[2] {
                variants.push([b0, b1, b2]);
            }
        }
    }
    variants
}

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
        let mut new_result = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < result.len() && j < list.len() {
            match result[i].cmp(&list[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    new_result.push(result[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        result = new_result;
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

    #[test]
    fn test_case_variants() {
        let variants = case_variants(*b"hel");
        assert_eq!(variants.len(), 8);
        assert!(variants.contains(b"hel"));
        assert!(variants.contains(b"HEL"));
        assert!(variants.contains(b"Hel"));
        assert!(variants.contains(b"hEL"));
    }

    #[test]
    fn test_case_variants_non_letter() {
        // Digits and symbols do not generate variants
        let variants = case_variants(*b"h1!");
        assert_eq!(variants.len(), 2); // h/H only
        assert!(variants.contains(b"h1!"));
        assert!(variants.contains(b"H1!"));
    }

    #[test]
    fn test_case_variants_all_non_letter() {
        let variants = case_variants(*b"123");
        assert_eq!(variants.len(), 1);
        assert!(variants.contains(b"123"));
    }
}
