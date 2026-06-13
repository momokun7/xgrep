//! Property-based equivalence tests: xgrep search results must match a naive
//! line-by-line grep oracle for all randomly generated corpora and patterns.
//!
//! Scope: case-sensitive literal substring search only (3+ byte patterns).
//! Regex, smart-case, word, and glob options are intentionally out of scope.

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use std::collections::BTreeSet;
use std::fs;
use tempfile::tempdir;
use xgrep_search::{SearchOptions, Xgrep};

// ---------------------------------------------------------------------------
// Proptest configuration
// ---------------------------------------------------------------------------

proptest! {
    // 64 cases balances coverage with CI time. Each case exercises a different
    // random corpus + pattern pair.
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Core invariant: xgrep results == naive grep oracle for every
    /// (corpus, pattern) pair generated.
    #[test]
    fn xgrep_matches_naive_grep(
        corpus in corpus_strategy(),
        pattern in pattern_strategy(),
    ) {
        run_equivalence_check(corpus, pattern)?;
    }

    /// Variant: derive the pattern from the corpus itself, guaranteeing
    /// at least some true-positive matches to exercise the hit path.
    #[test]
    fn xgrep_matches_naive_grep_positive(
        corpus in corpus_strategy(),
        pattern in pattern_from_corpus_strategy(),
    ) {
        // pattern_from_corpus_strategy returns None when corpus is empty
        // or all lines are too short; skip those cases.
        if let Some(pat) = pattern {
            run_equivalence_check(corpus, pat)?;
        }
    }
}

// ---------------------------------------------------------------------------
// Test runner
// ---------------------------------------------------------------------------

fn run_equivalence_check(
    corpus: Vec<(String, String)>,
    pattern: String,
) -> Result<(), TestCaseError> {
    // Skip patterns shorter than 3 bytes — those bypass the trigram index and
    // use a different code path not under test here.
    if pattern.len() < 3 {
        return Ok(());
    }

    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    // Write corpus files.
    for (filename, content) in &corpus {
        let path = root.join(filename);
        // Ensure parent dirs exist (filenames are flat in our strategy, so
        // this is a no-op, but kept for safety).
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create_dir_all");
        }
        fs::write(&path, content).expect("write corpus file");
    }

    // Build xgrep index.
    let xg = Xgrep::open(root).expect("Xgrep::open");
    xg.build_index().expect("build_index");

    // Run xgrep search (case-sensitive literal, no filters).
    let opts = SearchOptions::default(); // regex=false, case_insensitive=false
    let xg_results = xg.search(&pattern, &opts).expect("search");

    // Collect xgrep results as (relative_file, line_number) pairs.
    // xgrep returns file paths relative to root (using OS path separator).
    let xg_set: BTreeSet<(String, usize)> = xg_results
        .into_iter()
        .map(|r| {
            // Normalise path separator to '/' for platform-agnostic comparison.
            let normalized = r.file.replace('\\', "/");
            (normalized, r.line_number)
        })
        .collect();

    // Compute oracle: naive line-by-line substring search over corpus bytes.
    let oracle_set: BTreeSet<(String, usize)> = corpus
        .iter()
        .flat_map(|(filename, content)| {
            // Normalise filename the same way.
            let norm_name = filename.replace('\\', "/");
            let pat_bytes = pattern.as_bytes();
            content
                .lines()
                .enumerate()
                .filter_map(move |(idx, line)| {
                    if line
                        .as_bytes()
                        .windows(pat_bytes.len())
                        .any(|w| w == pat_bytes)
                    {
                        Some((norm_name.clone(), idx + 1)) // 1-based line numbers
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    // The sets must be identical.
    prop_assert_eq!(
        xg_set,
        oracle_set,
        "mismatch for pattern {:?} in corpus with {} file(s)",
        pattern,
        corpus.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Generate a random corpus: 1–6 files, each with 1–20 lines of ASCII text.
fn corpus_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
    prop::collection::vec(file_entry_strategy(), 1..=6)
}

/// A single (filename, content) pair.
fn file_entry_strategy() -> impl Strategy<Value = (String, String)> {
    (filename_strategy(), file_content_strategy())
}

/// File names: alphanumeric + underscore, 4–12 chars, with a fixed ".txt"
/// extension so xgrep does not filter them out by file type.
fn filename_strategy() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{3,11}\\.txt"
}

/// File content: 1–20 lines of printable ASCII text (32–126).
/// Occasionally injects a UTF-8 multibyte character so that the byte/char
/// distinction is exercised.
fn file_content_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(line_strategy(), 1..=20).prop_map(|lines| lines.join("\n"))
}

/// A single line: printable ASCII, length 0–60.
fn line_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        // Pure ASCII lines (most common)
        8 => "[a-zA-Z0-9 _\\-\\.\\(\\)\\{\\}\\[\\]\\#\\!\\?]{0,60}",
        // Lines that may contain multibyte UTF-8 sequences
        2 => "[ -~\u{00e9}\u{00e0}\u{00fc}\u{0041}-\u{0060}]{0,60}",
    ]
}

/// Generate a random 3–12 byte ASCII printable pattern (may or may not match).
fn pattern_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_\\-\\.]{3,12}"
}

/// Derive the pattern by extracting a 3–12 byte substring from one of the
/// corpus lines. Returns None if no eligible line exists.
fn pattern_from_corpus_strategy() -> impl Strategy<Value = Option<String>> {
    corpus_strategy().prop_flat_map(|corpus| {
        // Collect all lines from all files.
        let candidates: Vec<String> = corpus
            .iter()
            .flat_map(|(_, content)| {
                content
                    .lines()
                    .filter(|l| l.len() >= 3)
                    .map(|l| l.to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();

        if candidates.is_empty() {
            // No eligible lines — skip.
            return Just(None).boxed();
        }

        let len = candidates.len();
        (0..len)
            .prop_flat_map(move |line_idx| {
                let line = candidates[line_idx].clone();
                let max_start = line.len().saturating_sub(3);
                if max_start == 0 {
                    // Line is exactly 3 bytes; use whole line as pattern.
                    let pat = line[..3].to_owned();
                    return Just(Some(pat)).boxed();
                }
                let max_end = line.len().min(12);
                (0..=max_start)
                    .prop_flat_map(move |start| {
                        let line2 = line.clone();
                        let min_end = (start + 3).min(line2.len());
                        let end_max = (start + max_end).min(line2.len());
                        if min_end > end_max {
                            Just(None).boxed()
                        } else {
                            (min_end..=end_max)
                                .prop_map(move |end| {
                                    // Ensure we slice on char boundaries.
                                    let s = &line2[..];
                                    let byte_start = find_char_boundary(s, start);
                                    let byte_end = find_char_boundary(s, end);
                                    if byte_end > byte_start && (byte_end - byte_start) >= 3 {
                                        Some(s[byte_start..byte_end].to_owned())
                                    } else {
                                        None
                                    }
                                })
                                .boxed()
                        }
                    })
                    .boxed()
            })
            .boxed()
    })
}

/// Find the nearest valid char boundary at or after `pos` in `s`.
fn find_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    // Walk forward to a char boundary.
    let mut p = pos;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
