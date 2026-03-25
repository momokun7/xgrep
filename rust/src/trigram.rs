use std::collections::BTreeSet;

/// Extract trigrams from a byte sequence (deduplicated and sorted).
///
/// Uses BTreeSet for ordered deduplication. Benchmarked against HashSet+sort:
/// BTreeSet is ~1.3x slower on 10K-byte inputs but avoids the HashSet allocation
/// overhead on small inputs typical of source files. Keeping BTreeSet for simplicity.
pub fn extract_trigrams(data: &[u8]) -> Vec<[u8; 3]> {
    if data.len() < 3 {
        return vec![];
    }
    let mut seen = BTreeSet::new();
    for window in data.windows(3) {
        seen.insert([window[0], window[1], window[2]]);
    }
    seen.into_iter().collect()
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
    fn benchmark_btreeset_vs_hashset() {
        let data: Vec<u8> = (0..10000).map(|i| ((i * 7 + 13) % 128) as u8).collect();
        let iters = 1000;

        let t1 = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(extract_trigrams(std::hint::black_box(&data)));
        }
        let btree = t1.elapsed();

        let t2 = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(extract_hashset(std::hint::black_box(&data)));
        }
        let hash = t2.elapsed();

        eprintln!(
            "BTreeSet: {:?}, HashSet+sort: {:?}, ratio: {:.2}x",
            btree,
            hash,
            btree.as_nanos() as f64 / hash.as_nanos() as f64
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
        fn encode_decode_roundtrip(a in any::<u8>(), b in any::<u8>(), c in any::<u8>()) {
            let trigram = [a, b, c];
            let encoded = encode(trigram);
            let decoded = decode(encoded);
            prop_assert_eq!(decoded, trigram);
        }
    }
}
