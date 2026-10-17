//! Aider-style repository map.
//!
//! Files are nodes; a file that uses identifier `x` gets an edge to every
//! file defining `x`, weighted by `sqrt(uses)` and a few multipliers
//! (identifiers named in `focus_symbols` and long, specific names count more;
//! `_private` names and names defined in many files count less). PageRank
//! (optionally personalised to `focus` files) ranks files; each file's rank is
//! then split over its outgoing edges to rank `(defining file, identifier)`
//! pairs. The best-ranked definitions are printed as signatures, grouped by
//! file, until the token budget is used up.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{display_sig, FileTags, Kind};
use crate::mcp_render::estimate_tokens;

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 40;
/// Per (file, identifier): at most this many symbols are printed (e.g. many `new`).
const PER_NAME: usize = 3;

/// Inputs besides the files.
#[derive(Debug, Default, Clone)]
pub struct MapOptions {
    /// Personalisation: PageRank restarts at these files.
    pub focus: Vec<String>,
    /// Identifiers to weight up (x10), e.g. the ones in the task description.
    pub focus_symbols: Vec<String>,
    pub max_tokens: usize,
}

/// Rendered map plus statistics.
#[derive(Debug, Default)]
pub struct MapResult {
    pub text: String,
    pub files: usize,
    pub defs: usize,
    pub edges: usize,
    pub shown_symbols: usize,
    pub shown_files: usize,
    pub tokens: usize,
}

fn is_specific(name: &str) -> bool {
    name.len() >= 8
        && (name.contains('_')
            || (name.chars().any(|c| c.is_ascii_uppercase())
                && name.chars().any(|c| c.is_ascii_lowercase())))
}

/// Modules are mostly file/namespace declarations (`mod cli;`), impl blocks
/// repeat their type, headings are docs: none are useful map entries.
fn counts_as_def(kind: Kind) -> bool {
    !matches!(kind, Kind::Heading | Kind::Impl | Kind::Module)
}

/// Weight of a defining file's role: tests, examples and docs rarely belong
/// in an overview even when their helpers are used a lot.
fn role_weight(path: &str) -> f64 {
    use crate::rank::PathClass;
    match crate::rank::path_class(path) {
        PathClass::Source => 1.0,
        PathClass::Other => 0.5,
        PathClass::Example => 0.2,
        PathClass::Test => 0.1,
        PathClass::Docs | PathClass::Changelog | PathClass::Vendor => 0.05,
    }
}

/// Personalised PageRank over a weighted edge list.
pub fn pagerank(n: usize, edges: &[(u32, u32, f64)], personalization: &[f64]) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let mut out_w = vec![0.0f64; n];
    for &(r, _, w) in edges {
        out_w[r as usize] += w;
    }
    let mut rank = personalization.to_vec();
    let mut next = vec![0.0f64; n];
    for _ in 0..ITERATIONS {
        let dangling: f64 = (0..n).filter(|&i| out_w[i] == 0.0).map(|i| rank[i]).sum();
        for i in 0..n {
            next[i] = (1.0 - DAMPING + DAMPING * dangling) * personalization[i];
        }
        for &(r, d, w) in edges {
            let r = r as usize;
            next[d as usize] += DAMPING * rank[r] * w / out_w[r];
        }
        let delta: f64 = rank.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
        std::mem::swap(&mut rank, &mut next);
        if delta < 1e-9 {
            break;
        }
    }
    rank
}

/// Build the map for `files` (relative path, tags).
pub fn build_map(files: &[(String, Arc<FileTags>)], opts: &MapOptions) -> MapResult {
    let n = files.len();
    let mentioned: HashSet<&str> = opts.focus_symbols.iter().map(|s| s.as_str()).collect();
    // identifier -> files defining it
    let mut defines: HashMap<&str, Vec<u32>> = HashMap::new();
    let mut total_defs = 0usize;
    for (i, (_, t)) in files.iter().enumerate() {
        for s in &t.defs {
            if !counts_as_def(s.kind) {
                continue;
            }
            total_defs += 1;
            let v = defines.entry(s.name.as_str()).or_default();
            if v.last() != Some(&(i as u32)) {
                v.push(i as u32);
            }
        }
    }
    // edges (referencing file, defining file, weight) + per-edge identifier
    let mut edges: Vec<(u32, u32, f64)> = Vec::new();
    let mut edge_names: Vec<&str> = Vec::new();
    for (r, (_, t)) in files.iter().enumerate() {
        for (name, count) in &t.refs {
            let Some(ds) = defines.get(name.as_str()) else {
                continue;
            };
            let mut mul = 1.0f64;
            if mentioned.contains(name.as_str()) {
                mul *= 10.0;
            }
            if is_specific(name) {
                mul *= 10.0;
            }
            if name.starts_with('_') || name.chars().count() <= 3 {
                // private, or short and generic (`new`, `get`, `fmt`, `u32`)
                mul *= 0.1;
            }
            if ds.len() > 5 {
                mul *= 0.1;
            }
            let w = mul * (*count as f64).sqrt();
            for &d in ds {
                if d as usize == r {
                    continue;
                }
                edges.push((r as u32, d, w));
                edge_names.push(name.as_str());
            }
        }
    }
    // personalization
    let focus: HashSet<&str> = opts
        .focus
        .iter()
        .map(|s| s.trim_start_matches("./").trim_end_matches('/'))
        .collect();
    let mut pers = vec![0.0f64; n];
    let mut any_focus = false;
    if !focus.is_empty() {
        for (i, (p, _)) in files.iter().enumerate() {
            if focus.contains(p.as_str())
                || focus
                    .iter()
                    .any(|f| !f.is_empty() && p.starts_with(f) && p[f.len()..].starts_with('/'))
            {
                pers[i] = 1.0;
                any_focus = true;
            }
        }
    }
    if !any_focus {
        pers.iter_mut().for_each(|v| *v = 1.0);
    }
    let sum: f64 = pers.iter().sum();
    if sum > 0.0 {
        pers.iter_mut().for_each(|v| *v /= sum);
    }
    let rank = pagerank(n, &edges, &pers);

    // rank (defining file, identifier)
    let mut out_w = vec![0.0f64; n];
    for &(r, _, w) in &edges {
        out_w[r as usize] += w;
    }
    let mut def_rank: HashMap<(u32, &str), f64> = HashMap::new();
    for (k, &(r, d, w)) in edges.iter().enumerate() {
        *def_rank.entry((d, edge_names[k])).or_insert(0.0) +=
            rank[r as usize] * w / out_w[r as usize];
    }
    // Fallback when there are few edges: every top-level definition with the
    // file's rank, so small or isolated repos still get a map.
    for (i, (_, t)) in files.iter().enumerate() {
        for s in &t.defs {
            if s.parent.is_none() && counts_as_def(s.kind) {
                def_rank
                    .entry((i as u32, s.name.as_str()))
                    .or_insert(rank[i] * 1e-3);
            }
        }
    }
    for ((f, _), v) in def_rank.iter_mut() {
        *v *= role_weight(&files[*f as usize].0);
    }
    if any_focus {
        // Mentioned identifiers defined in focus files rank first.
        for ((f, name), v) in def_rank.iter_mut() {
            if mentioned.contains(name) {
                *v *= 10.0;
            }
            if pers[*f as usize] > 0.0 {
                *v *= 2.0;
            }
        }
    }
    let mut ranked: Vec<((u32, &str), f64)> = def_rank.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(files[a.0 .0 as usize].0.cmp(&files[b.0 .0 as usize].0))
            .then(a.0 .1.cmp(b.0 .1))
    });

    // Greedy selection under the token budget.
    let budget = opts.max_tokens.max(100);
    let mut used = 0usize;
    let mut chosen: HashMap<u32, HashSet<usize>> = HashMap::new(); // file -> symbol indices (incl. ancestors)
    let mut file_order: Vec<u32> = Vec::new();
    let mut shown_symbols = 0usize;
    let mut misses = 0usize;
    for ((f, name), _) in &ranked {
        let t = &files[*f as usize].1;
        let mut new_lines: Vec<usize> = Vec::new();
        let have = chosen.get(f);
        let mut picked = 0;
        for (idx, s) in t.defs.iter().enumerate() {
            if s.name != *name || s.kind == Kind::Heading || picked >= PER_NAME {
                continue;
            }
            picked += 1;
            // the symbol and its not-yet-shown ancestors
            let mut cur = Some(idx);
            while let Some(c) = cur {
                if have.is_some_and(|h| h.contains(&c)) || new_lines.contains(&c) {
                    break;
                }
                new_lines.push(c);
                cur = t.defs[c].parent.map(|p| p as usize);
            }
        }
        if new_lines.is_empty() {
            continue;
        }
        let mut cost: usize = new_lines
            .iter()
            .map(|&i| estimate_tokens(&line_for(t, i)))
            .sum();
        if have.is_none() {
            cost += estimate_tokens(&files[*f as usize].0) + 1;
        }
        if used + cost > budget {
            misses += 1;
            if misses > 50 {
                break;
            }
            continue;
        }
        used += cost;
        shown_symbols += 1;
        if have.is_none() {
            file_order.push(*f);
        }
        chosen.entry(*f).or_default().extend(new_lines);
    }

    let mut text = String::new();
    for f in &file_order {
        let (path, t) = &files[*f as usize];
        text.push_str(path);
        text.push_str(":\n");
        let mut idxs: Vec<usize> = chosen[f].iter().copied().collect();
        idxs.sort_by_key(|&i| (t.defs[i].start, t.defs[i].line, i));
        for i in idxs {
            text.push_str(&line_for(t, i));
        }
    }
    MapResult {
        tokens: estimate_tokens(&text),
        text,
        files: n,
        defs: total_defs,
        edges: edges.len(),
        shown_symbols,
        shown_files: file_order.len(),
    }
}

fn line_for(t: &FileTags, i: usize) -> String {
    let s = &t.defs[i];
    format!(
        "{}L{} {}\n",
        "  ".repeat(t.depth(i) + 1),
        s.line,
        display_sig(s)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{finalize_defs, Symbol};

    fn ft(defs: &[(&str, Kind, u32, u32)], refs: &[(&str, u32)]) -> Arc<FileTags> {
        let mut d: Vec<Symbol> = defs
            .iter()
            .map(|(n, k, s, e)| Symbol {
                kind: *k,
                name: n.to_string(),
                line: *s,
                start: *s,
                end: *e,
                parent: None,
                sig: format!("{} {}()", k.label(), n),
            })
            .collect();
        finalize_defs(&mut d);
        Arc::new(FileTags {
            defs: d,
            refs: refs.iter().map(|(n, c)| (n.to_string(), *c)).collect(),
            lines: 100,
        })
    }

    #[test]
    fn pagerank_sums_to_one_and_prefers_hubs() {
        let edges = vec![(0, 2, 1.0), (1, 2, 1.0), (2, 0, 1.0)];
        let r = pagerank(3, &edges, &[1.0 / 3.0; 3]);
        let s: f64 = r.iter().sum();
        assert!((s - 1.0).abs() < 1e-6, "{}", s);
        assert!(r[2] > r[0] && r[2] > r[1]);
    }

    #[test]
    fn map_ranks_widely_used_definitions_first() {
        let files = vec![
            (
                "src/core.rs".to_string(),
                ft(
                    &[
                        ("core_engine", Kind::Function, 1, 10),
                        ("rare_helper", Kind::Function, 12, 14),
                    ],
                    &[],
                ),
            ),
            (
                "src/a.rs".to_string(),
                ft(&[("a", Kind::Function, 1, 3)], &[("core_engine", 3)]),
            ),
            (
                "src/b.rs".to_string(),
                ft(&[("b", Kind::Function, 1, 3)], &[("core_engine", 1)]),
            ),
            (
                "src/c.rs".to_string(),
                ft(
                    &[("c", Kind::Function, 1, 3)],
                    &[("core_engine", 2), ("rare_helper", 1)],
                ),
            ),
        ];
        let m = build_map(
            &files,
            &MapOptions {
                max_tokens: 1000,
                ..Default::default()
            },
        );
        assert!(m.text.starts_with("src/core.rs:\n"), "{}", m.text);
        let first = m.text.lines().nth(1).unwrap();
        assert!(first.contains("core_engine"), "{}", m.text);
        assert!(m.edges >= 4);
        // tiny budget: only the best symbol fits
        let small = build_map(
            &files,
            &MapOptions {
                max_tokens: 100,
                ..Default::default()
            },
        );
        assert!(small.tokens <= 100);
        assert!(small.text.contains("core_engine"));
    }

    #[test]
    fn focus_personalizes() {
        let files = vec![
            (
                "x/one.rs".to_string(),
                ft(&[("one_thing", Kind::Function, 1, 2)], &[("two_thing", 1)]),
            ),
            (
                "y/two.rs".to_string(),
                ft(&[("two_thing", Kind::Function, 1, 2)], &[]),
            ),
            (
                "z/three.rs".to_string(),
                ft(
                    &[("three_thing", Kind::Function, 1, 2)],
                    &[("four_thing", 1)],
                ),
            ),
            (
                "w/four.rs".to_string(),
                ft(&[("four_thing", Kind::Function, 1, 2)], &[]),
            ),
        ];
        let m = build_map(
            &files,
            &MapOptions {
                focus: vec!["z/three.rs".into()],
                max_tokens: 1000,
                ..Default::default()
            },
        );
        let pos4 = m.text.find("four_thing").unwrap();
        let pos2 = m.text.find("two_thing").unwrap();
        assert!(pos4 < pos2, "{}", m.text);
    }

    #[test]
    fn nested_symbols_show_their_container() {
        let files = vec![
            (
                "a.rs".to_string(),
                ft(
                    &[
                        ("Store", Kind::Impl, 1, 20),
                        ("insert_item", Kind::Function, 5, 9),
                    ],
                    &[],
                ),
            ),
            ("b.rs".to_string(), ft(&[], &[("insert_item", 4)])),
        ];
        let m = build_map(
            &files,
            &MapOptions {
                max_tokens: 500,
                ..Default::default()
            },
        );
        assert!(
            m.text
                .contains("  L1 impl Store()\n    L5 fn insert_item()"),
            "{}",
            m.text
        );
    }
}
