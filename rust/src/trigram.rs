use std::cell::RefCell;

/// Number of distinct trigrams (24-bit space).
const TRIGRAM_SPACE: usize = 1 << 24;
/// Number of u64 words needed for a bitset over the whole trigram space (2 MiB).
const BITSET_WORDS: usize = TRIGRAM_SPACE / 64;
/// Inputs shorter than this use sort+dedup instead of the bitset.
const SMALL_INPUT: usize = 512;
/// When more distinct trigrams than this are found, emitting them by scanning the
/// bitset (fixed 2 MiB pass) is cheaper than sorting the list of hits.
const SCAN_THRESHOLD: usize = BITSET_WORDS / 4;

thread_local! {
    /// Reusable per-thread bitset over the 2^24 trigram space. Always all-zero
    /// between calls (every set bit is cleared before `extract_trigrams` returns).
    static SEEN: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

#[inline]
fn window_u32(data: &[u8], i: usize) -> u32 {
    (data[i] as u32) << 16 | (data[i + 1] as u32) << 8 | data[i + 2] as u32
}

/// Extract trigrams from a byte sequence (deduplicated and sorted).
///
/// Uses a reusable thread-local 2^24-bit bitset (2 MiB per thread) for
/// deduplication instead of a hashed set, which is ~5x faster on source code.
pub fn extract_trigrams(data: &[u8]) -> Vec<[u8; 3]> {
    extract_trigrams_u32(data).into_iter().map(decode).collect()
}

/// Same as [`extract_trigrams`] but returns trigrams encoded as u32
/// (see [`encode`]), sorted ascending and deduplicated.
pub fn extract_trigrams_u32(data: &[u8]) -> Vec<u32> {
    if data.len() < 3 {
        return vec![];
    }
    let n = data.len() - 2;
    if data.len() < SMALL_INPUT {
        let mut v: Vec<u32> = (0..n).map(|i| window_u32(data, i)).collect();
        v.sort_unstable();
        v.dedup();
        return v;
    }
    SEEN.with(|cell| {
        let mut bits = cell.borrow_mut();
        if bits.len() != BITSET_WORDS {
            *bits = vec![0u64; BITSET_WORDS];
        }
        let mut found: Vec<u32> = Vec::with_capacity(n.min(4096));
        let mut t: u32 = (data[0] as u32) << 8 | data[1] as u32;
        for &b in &data[2..] {
            t = ((t << 8) | b as u32) & 0x00FF_FFFF;
            let w = (t >> 6) as usize;
            let bit = 1u64 << (t & 63);
            // t < 2^24, so w < 2^18 == bits.len().
            let word = &mut bits[w];
            if *word & bit == 0 {
                *word |= bit;
                found.push(t);
            }
        }
        if found.len() > SCAN_THRESHOLD {
            // Dense: walk the bitset in order (already sorted), clearing as we go.
            found.clear();
            for (w, word) in bits.iter_mut().enumerate() {
                let mut x = *word;
                if x == 0 {
                    continue;
                }
                *word = 0;
                while x != 0 {
                    let tz = x.trailing_zeros();
                    found.push(((w as u32) << 6) | tz);
                    x &= x - 1;
                }
            }
        } else {
            for &t in &found {
                bits[(t >> 6) as usize] = 0;
            }
            found.sort_unstable();
        }
        found
    })
}

/// Encode a trigram ([u8; 3]) to u32 (upper byte is 0).
#[allow(dead_code)]
pub fn encode(trigram: [u8; 3]) -> u32 {
    (trigram[0] as u32) << 16 | (trigram[1] as u32) << 8 | trigram[2] as u32
}

/// Decode a u32 to [u8; 3].
#[allow(dead_code)]
pub fn decode(value: u32) -> [u8; 3] {
    [(value >> 16) as u8, (value >> 8) as u8, value as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_trigrams() {
        let trigrams = extract_trigrams(b"hello");
        assert_eq!(trigrams, vec![*b"ell", *b"hel", *b"llo"]);
    }

    #[test]
    fn test_extract_short_input() {
        assert_eq!(extract_trigrams(b"ab").len(), 0);
        assert_eq!(extract_trigrams(b"abc"), vec![*b"abc"]);
    }

    #[test]
    fn test_extract_dedup() {
        let trigrams = extract_trigrams(b"aaaa");
        assert_eq!(trigrams, vec![*b"aaa"]);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let t = *b"abc";
        assert_eq!(decode(encode(t)), t);
    }

    #[test]
    fn test_extract_empty() {
        assert!(extract_trigrams(b"").is_empty());
    }

    #[test]
    fn test_extract_single_byte() {
        assert!(extract_trigrams(b"a").is_empty());
    }

    #[test]
    fn test_extract_exactly_3_bytes() {
        let result = extract_trigrams(b"abc");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], *b"abc");
    }

    #[test]
    fn test_extract_trigrams_sorted() {
        let result = extract_trigrams(b"zab");
        // Should be sorted: "abz" would not exist, "zab" is the only trigram
        assert_eq!(result, vec![*b"zab"]);
    }

    #[test]
    fn test_extract_large_matches_reference() {
        // Exercise both the bitset path and the dense-scan path.
        let sparse: Vec<u8> = (0..10_000u32).map(|i| ((i * 7 + 13) % 128) as u8).collect();
        let dense: Vec<u8> = (0..400_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for data in [&sparse[..], &dense[..]] {
            let mut expect: Vec<[u8; 3]> = data.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
            expect.sort_unstable();
            expect.dedup();
            assert_eq!(extract_trigrams(data), expect);
            // Calling again must give the same answer (bitset left clean).
            assert_eq!(extract_trigrams(data), expect);
        }
    }

    #[test]
    fn test_extract_utf8_bytes() {
        // UTF-8 multibyte: trigrams are byte-level
        let data = "あいう".as_bytes(); // 9 bytes (3 chars * 3 bytes each)
        let result = extract_trigrams(data);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_zero() {
        assert_eq!(encode([0, 0, 0]), 0);
    }

    #[test]
    fn test_encode_max() {
        assert_eq!(encode([0xFF, 0xFF, 0xFF]), 0x00FFFFFF);
    }
}

#[cfg(test)]
mod bench_tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Instant;

    fn extract_hashset(data: &[u8]) -> Vec<[u8; 3]> {
        if data.len() < 3 {
            return vec![];
        }
        let mut seen = HashSet::new();
        for w in data.windows(3) {
            seen.insert([w[0], w[1], w[2]]);
        }
        let mut r: Vec<_> = seen.into_iter().collect();
        r.sort();
        r
    }

    #[test]
    fn benchmark_bitset_vs_hashset() {
        let data: Vec<u8> = (0..10000).map(|i| ((i * 7 + 13) % 128) as u8).collect();
        let iters = 1000;

        let t1 = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(extract_trigrams(std::hint::black_box(&data)));
        }
        let bitset = t1.elapsed();

        let t2 = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(extract_hashset(std::hint::black_box(&data)));
        }
        let hash = t2.elapsed();

        eprintln!(
            "bitset: {:?}, HashSet+sort: {:?}, ratio: {:.2}x",
            bitset,
            hash,
            bitset.as_nanos() as f64 / hash.as_nanos() as f64
        );

        assert_eq!(extract_trigrams(&data), extract_hashset(&data));
    }
}

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn extract_trigrams_length(input in prop::collection::vec(any::<u8>(), 0..200)) {
            let trigrams = extract_trigrams(&input);
            if input.len() < 3 {
                prop_assert!(trigrams.is_empty());
            } else {
                prop_assert!(trigrams.len() <= input.len() - 2);
            }
        }

        #[test]
        fn extract_trigrams_are_subsequences(input in prop::collection::vec(any::<u8>(), 3..100)) {
            let trigrams = extract_trigrams(&input);
            for t in &trigrams {
                let found = input.windows(3).any(|w| w == t);
                prop_assert!(found, "Trigram {:?} not found in input", t);
            }
        }

        #[test]
        fn extract_trigrams_matches_reference(input in prop::collection::vec(any::<u8>(), 0..3000)) {
            let mut expect: Vec<[u8; 3]> = input.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
            expect.sort_unstable();
            expect.dedup();
            prop_assert_eq!(extract_trigrams(&input), expect);
        }

        #[test]
        fn encode_decode_roundtrip(a in any::<u8>(), b in any::<u8>(), c in any::<u8>()) {
            let trigram = [a, b, c];
            let encoded = encode(trigram);
            let decoded = decode(encoded);
            prop_assert_eq!(decoded, trigram);
        }
    }
}
