//! Per-server-session memory of what has already been returned to the agent.
//!
//! The MCP server records every `(file, line range)` it renders (search
//! blocks, definitions, `read_file` ranges). Later results that fall inside a
//! recorded range are collapsed to a one-line `already shown: path:L10-40`
//! pointer instead of being repeated. `session_reset` clears the memory and
//! `include_seen: true` bypasses it for one call.

use std::collections::HashMap;

/// Seen line ranges per file (inclusive, 1-based).
#[derive(Debug, Default)]
pub struct Session {
    seen: HashMap<String, Vec<(usize, usize)>>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything. Returns the number of files that were tracked.
    pub fn reset(&mut self) -> usize {
        let n = self.seen.len();
        self.seen.clear();
        n
    }

    /// The recorded range containing `line` in `file`, if any.
    pub fn seen_range(&self, file: &str, line: usize) -> Option<(usize, usize)> {
        self.seen
            .get(file)?
            .iter()
            .find(|(s, e)| *s <= line && line <= *e)
            .copied()
    }

    /// Record `start..=end` of `file` as shown. Overlapping/adjacent ranges are merged.
    pub fn mark(&mut self, file: &str, start: usize, end: usize) {
        if end < start {
            return;
        }
        let ranges = self.seen.entry(file.to_string()).or_default();
        ranges.push((start, end));
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for &(s, e) in ranges.iter() {
            match merged.last_mut() {
                Some(last) if s <= last.1 + 1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        *ranges = merged;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_merges_and_finds() {
        let mut s = Session::new();
        s.mark("a.rs", 10, 20);
        s.mark("a.rs", 21, 25);
        s.mark("a.rs", 40, 41);
        assert_eq!(s.seen_range("a.rs", 22), Some((10, 25)));
        assert_eq!(s.seen_range("a.rs", 30), None);
        assert_eq!(s.seen_range("b.rs", 10), None);
        assert_eq!(s.reset(), 1);
        assert_eq!(s.seen_range("a.rs", 22), None);
    }
}
