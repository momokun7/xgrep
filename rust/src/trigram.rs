use std::cell::RefCell;

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

// ============================================================
// Folded trigrams with per-(trigram, file) masks (index format v4)
// ============================================================
//
// The index stores *case-folded* trigrams: every byte is ASCII-lowercased
// before the trigram is formed (non-ASCII bytes are kept as is). A file that
// contains a string `s` byte-for-byte therefore always contains every folded
// trigram of `fold(s)`, so folded lookups are a sound (superset) filter for
// both case-sensitive and ASCII-case-insensitive search; the final content
// match removes the extra candidates.
//
// For every (folded trigram, file) pair two 8-bit masks are recorded (the
// idea comes from Cursor's local regex index):
//
// * `loc`  — bit `p % 8` is set for every position `p` where the trigram
//   starts in the file.
// * `next` — a tiny bloom filter of the (folded) bytes that follow the
//   trigram: bit [`next_bit`]`(b)` is set for every following byte `b`.
//
// Each file is also treated as if a virtual NUL ([`SENTINEL`]) followed its
// last byte, so a file ending in `ab` contains the trigram `ab\0`. Indexed
// files never contain NUL (they would be binary), so this never collides with
// real content, and it guarantees that *every* occurrence of a 2-byte string
// `ab` produces some trigram with prefix `ab` (see `lookup_trigram_prefix`).

/// Virtual byte appended after the last byte of every indexed file.
pub const SENTINEL: u8 = 0;

/// ASCII case folding used for index trigrams.
#[inline]
pub fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// Fold a trigram (see [`fold`]).
#[inline]
pub fn fold_trigram(t: [u8; 3]) -> [u8; 3] {
    [fold(t[0]), fold(t[1]), fold(t[2])]
}

/// `next` mask bit for a following (already folded) byte.
///
/// The low three bits spread the bytes that are common in source code
/// (letters, digits, punctuation) best among the simple hashes we measured.
#[inline]
pub fn next_bit(folded: u8) -> u8 {
    1 << (folded & 7)
}

/// `loc` mask bit for a trigram starting at byte offset `pos`.
#[inline]
pub fn loc_bit(pos: usize) -> u8 {
    1 << (pos & 7)
}

/// A folded trigram of one file with its `loc` / `next` masks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Gram {
    pub tri: [u8; 3],
    pub loc: u8,
    pub next: u8,
}

impl Gram {
    /// The trigram encoded as u32 (see [`encode`]).
    #[inline]
    pub fn key(&self) -> u32 {
        encode(self.tri)
    }
}

/// Open-addressing hash table (trigram → masks) reused per thread.
///
/// Each slot packs `trigram << 16 | next << 8 | loc` into one u64, so a probe
/// touches a single word.
struct MaskTable {
    slots: Vec<u64>,
    /// Occupied slot indices, in insertion order.
    used: Vec<u32>,
    /// Reused buffer for sorting the drained entries.
    sorted: Vec<u64>,
    shift: u32,
}

const EMPTY_SLOT: u64 = u64::MAX;
/// Tables with more slots than this are released after use.
const MASK_TABLE_KEEP: usize = 1 << 17;

impl MaskTable {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            used: Vec::new(),
            sorted: Vec::new(),
            shift: 32,
        }
    }

    fn reset(&mut self, expected: usize) {
        let cap = (expected.min(1 << 20) * 2).next_power_of_two().max(256);
        // A larger (clean) table from an earlier call is reused as is.
        if self.slots.len() < cap {
            self.slots = vec![EMPTY_SLOT; cap];
            self.shift = 32 - cap.trailing_zeros();
        }
        self.used.clear();
    }

    #[inline]
    fn slot(&self, key: u32) -> usize {
        (key.wrapping_mul(0x9E37_79B1) >> self.shift) as usize
    }

    #[inline]
    fn or_insert(&mut self, key: u32, val: u16) {
        let mask = self.slots.len() - 1;
        let mut i = self.slot(key);
        loop {
            let s = self.slots[i];
            if s >> 16 == key as u64 {
                self.slots[i] = s | val as u64;
                return;
            }
            if s == EMPTY_SLOT {
                self.slots[i] = (key as u64) << 16 | val as u64;
                self.used.push(i as u32);
                if self.used.len() * 2 > self.slots.len() {
                    self.grow();
                }
                return;
            }
            i = (i + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let old = std::mem::take(&mut self.slots);
        let old_used = std::mem::take(&mut self.used);
        let cap = old.len() * 2;
        self.slots = vec![EMPTY_SLOT; cap];
        self.shift = 32 - cap.trailing_zeros();
        self.used = Vec::with_capacity(old_used.len() * 2);
        let mask = cap - 1;
        for &u in &old_used {
            let s = old[u as usize];
            let mut i = self.slot((s >> 16) as u32);
            while self.slots[i] != EMPTY_SLOT {
                i = (i + 1) & mask;
            }
            self.slots[i] = s;
            self.used.push(i as u32);
        }
    }

    /// Drain into sorted grams and leave the table empty.
    fn drain_sorted(&mut self) -> Vec<Gram> {
        let mut packed = std::mem::take(&mut self.sorted);
        packed.clear();
        packed.extend(
            self.used
                .iter()
                .map(|&u| std::mem::replace(&mut self.slots[u as usize], EMPTY_SLOT)),
        );
        self.used.clear();
        packed.sort_unstable();
        let out = packed
            .iter()
            .map(|&p| Gram {
                tri: decode((p >> 16) as u32),
                loc: p as u8,
                next: (p >> 8) as u8,
            })
            .collect();
        if self.slots.len() > MASK_TABLE_KEEP {
            *self = Self::new();
        } else {
            self.sorted = packed;
        }
        out
    }
}

thread_local! {
    static MASKS: RefCell<MaskTable> = const { RefCell::new(MaskTable::new()) };
}

/// Extract the folded trigrams of a file with their `loc` / `next` masks,
/// sorted by trigram (see the section comment above for the semantics).
///
/// The content is treated as `data` followed by one [`SENTINEL`] byte, so a
/// file of `n >= 2` bytes yields `n - 1` trigram occurrences.
pub fn extract_grams(data: &[u8]) -> Vec<Gram> {
    let n = data.len();
    if n < 2 {
        return Vec::new();
    }
    MASKS.with(|cell| {
        let mut table = cell.borrow_mut();
        table.reset(n.min(1 << 14));
        let mut t: u32 = (fold(data[0]) as u32) << 8 | fold(data[1]) as u32;
        for p in 0..n - 1 {
            let c = if p + 2 < n {
                fold(data[p + 2])
            } else {
                SENTINEL
            };
            t = ((t << 8) | c as u32) & 0x00FF_FFFF;
            let next = if p + 3 < n {
                next_bit(fold(data[p + 3]))
            } else {
                0
            };
            table.or_insert(t, loc_bit(p) as u16 | (next as u16) << 8);
        }
        table.drain_sorted()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let t = *b"abc";
        assert_eq!(decode(encode(t)), t);
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
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    /// Straightforward reference for [`extract_grams`].
    fn grams_reference(data: &[u8]) -> Vec<Gram> {
        let mut v: Vec<u8> = data.iter().map(|&b| fold(b)).collect();
        v.push(SENTINEL);
        let mut map: std::collections::BTreeMap<[u8; 3], (u8, u8)> = Default::default();
        if data.len() >= 2 {
            for p in 0..v.len() - 2 {
                let e = map.entry([v[p], v[p + 1], v[p + 2]]).or_default();
                e.0 |= 1 << (p % 8);
                if p + 3 < data.len() {
                    e.1 |= 1 << (v[p + 3] % 8);
                }
            }
        }
        map.into_iter()
            .map(|(tri, (loc, next))| Gram { tri, loc, next })
            .collect()
    }

    #[test]
    fn grams_small_examples() {
        assert!(extract_grams(b"").is_empty());
        assert!(extract_grams(b"a").is_empty());
        // "aB" -> folded "ab" + sentinel.
        assert_eq!(
            extract_grams(b"aB"),
            vec![Gram {
                tri: *b"ab\0",
                loc: 1,
                next: 0
            }]
        );
        // "Abab": abA? folded "abab\0": "aba"@0 (next b), "bab"@1 (next \0 -> none), "ab\0"@2
        let g = extract_grams(b"Abab");
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].tri, *b"ab\0");
        assert_eq!(g[0].loc, 1 << 2);
        assert_eq!(g[1].tri, *b"aba");
        assert_eq!(g[1].next, next_bit(b'b'));
        assert_eq!(g[2].tri, *b"bab");
        assert_eq!(g[2].next, 0);
    }

    #[test]
    fn grams_large_matches_reference() {
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert_eq!(extract_grams(&data), grams_reference(&data));
        // Table state must be clean afterwards.
        assert_eq!(extract_grams(b"xyzw"), grams_reference(b"xyzw"));
    }

    proptest! {
        #[test]
        fn extract_grams_matches_reference(input in prop::collection::vec(
            prop::sample::select(b"aAbBcC xyz\n0_".to_vec()), 0..600)) {
            prop_assert_eq!(extract_grams(&input), grams_reference(&input));
        }

        #[test]
        fn extract_grams_matches_reference_bytes(input in prop::collection::vec(any::<u8>(), 0..3000)) {
            prop_assert_eq!(extract_grams(&input), grams_reference(&input));
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
