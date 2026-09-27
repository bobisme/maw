//! Bounded path domain for exhaustive path-predicate tests (bn-2wav).
//!
//! The production validators in `recover`, `undo` and `capture` are proven
//! by enumerating every input in this domain — a complete proof for the
//! bound. The lexical cores are additionally Kani-proven in
//! `maw_core::model::path_safety`.

/// Segments in the domain: a name, a name sharing a prefix with it, `.`,
/// `..`, and the empty segment (repeated / trailing / leading slashes).
pub const SEGS: [&str; 5] = ["a", "ab", ".", "..", ""];

/// Every `/`-joined string of 1..=`max` segments from [`SEGS`], each with and
/// without a leading `/`, deduplicated.
pub fn bounded_paths(max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut layer: Vec<Vec<&str>> = vec![vec![]];
    for _ in 0..max {
        let mut next = Vec::new();
        for prefix in &layer {
            for seg in SEGS {
                let mut v = prefix.clone();
                v.push(seg);
                next.push(v);
            }
        }
        for v in &next {
            let s = v.join("/");
            out.push(format!("/{s}"));
            out.push(s);
        }
        layer = next;
    }
    out.sort();
    out.dedup();
    out
}

/// Independent spec: does this `/`-separated string name something strictly
/// inside a base it is joined onto? (Unrooted, no `..` segment, and at least
/// one segment that is neither empty nor `.`.)
pub fn spec_strictly_inside(s: &str) -> bool {
    !s.starts_with('/')
        && !s.split('/').any(|seg| seg == "..")
        && s.split('/').any(|seg| !seg.is_empty() && seg != ".")
}
