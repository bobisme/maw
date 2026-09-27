//! Lexical path-safety predicates (bn-2wav).
//!
//! These are the pure, filesystem-free halves of the validators that guard
//! recovery (`maw ws recover --show` / `--restore-file`), undo's removal of
//! added paths, and capture's embedded-repo exclusion filter. Callers keep
//! their own filesystem checks (e.g. `--restore-file` still refuses symlinked
//! parents via `symlink_metadata`); what lives here is only the component
//! inspection, so it can be proven:
//!
//! - with Kani over the byte / component-kind level (`kani_proofs` below), and
//! - by exhaustive enumeration over real `std::path::Path` values in unit
//!   tests (a complete proof for the enumerated bound).
//!
//! Out of scope: `workspace::path_is_within` in maw-cli canonicalizes through
//! the OS, so it is not a lexical predicate and is not covered here.

use std::fmt;
use std::path::{Component, Path};

/// The kind of one `std::path::Component`, without its payload.
///
/// This is the domain the containment predicate is proven over: the
/// predicate never looks at a component's bytes, only at its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexComp {
    /// Windows drive / UNC prefix.
    Prefix,
    /// A leading root separator (`/`).
    RootDir,
    /// A leading `.` (std drops interior `.` components).
    CurDir,
    /// `..`.
    ParentDir,
    /// Any ordinary name.
    Normal,
}

impl LexComp {
    /// Classify a real `std::path::Component`.
    #[must_use]
    pub const fn of(component: &Component<'_>) -> Self {
        match component {
            Component::Prefix(_) => Self::Prefix,
            Component::RootDir => Self::RootDir,
            Component::CurDir => Self::CurDir,
            Component::ParentDir => Self::ParentDir,
            Component::Normal(_) => Self::Normal,
        }
    }
}

/// True iff a path made of these components, joined onto any base
/// directory, names something *strictly inside* that base:
/// every component is `Normal` or `CurDir`, and at least one is `Normal`.
///
/// Rejects absolute paths (`RootDir` / `Prefix`), any `..`, and paths that
/// resolve to the base itself (`""`, `"."`, `"./"`) — the last class matters
/// for callers that delete or overwrite the joined path.
pub fn is_contained_relative<I>(components: I) -> bool
where
    I: IntoIterator<Item = LexComp>,
{
    let mut saw_normal = false;
    for component in components {
        match component {
            LexComp::Normal => saw_normal = true,
            LexComp::CurDir => {}
            LexComp::Prefix | LexComp::RootDir | LexComp::ParentDir => return false,
        }
    }
    saw_normal
}

/// [`is_contained_relative`] over a real `Path`'s components.
#[must_use]
pub fn is_contained_relative_path(path: &Path) -> bool {
    is_contained_relative(path.components().map(|c| LexComp::of(&c)))
}

/// Why a `--show` / `--restore-file` path argument was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShowPathError {
    Empty,
    Absolute,
    NulByte,
    Traversal,
}

impl fmt::Display for ShowPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "Path cannot be empty",
            Self::Absolute => "Path must be relative (no leading '/')",
            Self::NulByte => "Path cannot contain null bytes",
            Self::Traversal => "Path cannot contain '..' components (directory traversal)",
        })
    }
}

impl std::error::Error for ShowPathError {}

/// Validate a repo-relative path argument (`recover --show` /
/// `--restore-file`) against traversal: non-empty, no leading `/`, no NUL,
/// and no `..` segment (segments split on `/`).
///
/// # Errors
/// Returns the first violated rule, checked in the order above.
pub const fn validate_show_path(path: &str) -> Result<(), ShowPathError> {
    validate_show_path_bytes(path.as_bytes())
}

/// Byte-level core of [`validate_show_path`].
///
/// # Errors
/// See [`validate_show_path`].
pub const fn validate_show_path_bytes(path: &[u8]) -> Result<(), ShowPathError> {
    if path.is_empty() {
        return Err(ShowPathError::Empty);
    }
    if path[0] == b'/' {
        return Err(ShowPathError::Absolute);
    }
    let mut i = 0;
    while i < path.len() {
        if path[i] == 0 {
            return Err(ShowPathError::NulByte);
        }
        i += 1;
    }
    let mut start = 0;
    let mut i = 0;
    while i <= path.len() {
        if i == path.len() || path[i] == b'/' {
            if i - start == 2 && path[start] == b'.' && path[start + 1] == b'.' {
                return Err(ShowPathError::Traversal);
            }
            start = i + 1;
        }
        i += 1;
    }
    Ok(())
}

/// Return the next non-empty `/`-separated segment at or after `from` as a
/// half-open byte range.
const fn next_segment(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut start = from;
    while start < bytes.len() && bytes[start] == b'/' {
        start += 1;
    }
    if start >= bytes.len() {
        return None;
    }
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'/' {
        end += 1;
    }
    Some((start, end))
}

/// Component-wise "is `path` at or under directory `dir`".
///
/// Both arguments are `/`-separated repo paths; empty segments (from
/// repeated or trailing slashes) are ignored. `a` covers `a` and `a/x` but
/// never `ab`. An empty `dir` (no segments) covers nothing.
#[must_use]
pub const fn path_is_under(path: &str, dir: &str) -> bool {
    path_bytes_under(path.as_bytes(), dir.as_bytes())
}

/// Byte-level core of [`path_is_under`].
#[must_use]
pub const fn path_bytes_under(path: &[u8], dir: &[u8]) -> bool {
    let mut pi = 0;
    let mut di = 0;
    let mut matched_any = false;
    loop {
        let Some((ds, de)) = next_segment(dir, di) else {
            return matched_any;
        };
        let Some((ps, pe)) = next_segment(path, pi) else {
            return false;
        };
        if de - ds != pe - ps {
            return false;
        }
        let mut k = 0;
        while k < de - ds {
            if dir[ds + k] != path[ps + k] {
                return false;
            }
            k += 1;
        }
        matched_any = true;
        di = de;
        pi = pe;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Segments allowed in the enumerated domain. Together with an optional
    /// leading `/` this covers absolute, empty, `.`, `..`, the `a`/`ab`
    /// prefix pair and multi-component (`a/b`) paths.
    const SEGS: [&str; 5] = ["a", "ab", ".", "..", ""];

    /// Every `/`-joined string of 1..=`max` segments from [`SEGS`], with and
    /// without a leading `/`.
    fn all_paths(max: usize) -> Vec<String> {
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

    /// Lexically resolve `rel` against `base`: returns the resolved
    /// component stack, or `None` when it escapes `base` (root reset or
    /// `..` above the base).
    fn lexical_resolve(rel: &Path) -> Option<Vec<String>> {
        let mut stack: Vec<String> = Vec::new();
        for c in rel.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => return None,
                Component::CurDir => {}
                Component::ParentDir => {
                    stack.pop()?;
                }
                Component::Normal(n) => stack.push(n.to_string_lossy().into_owned()),
            }
        }
        Some(stack)
    }

    /// Exhaustive over every path of <= 4 segments from [`SEGS`] (optionally
    /// rooted): `is_contained_relative_path` accepts exactly the paths whose
    /// join onto a base stays strictly inside it with no `..` component.
    #[test]
    fn exhaustive_contained_relative_path_le_4_segments() {
        let base = PathBuf::from("/base/ws");
        let mut accepted = 0;
        for s in all_paths(4) {
            let p = Path::new(&s);
            let ok = is_contained_relative_path(p);
            let has_parent = p.components().any(|c| c == Component::ParentDir);
            let resolved = lexical_resolve(p);
            let strictly_inside =
                !has_parent && resolved.as_ref().is_some_and(|stack| !stack.is_empty());
            assert_eq!(ok, strictly_inside, "{s:?}");
            if ok {
                accepted += 1;
                let joined = base.join(p);
                assert!(joined.starts_with(&base), "{s:?} -> {joined:?}");
                let rest = joined.strip_prefix(&base).expect("under base");
                assert!(
                    rest.components().next().is_some()
                        && rest.components().all(|c| matches!(c, Component::Normal(_))),
                    "{s:?} -> {rest:?}"
                );
            }
        }
        assert!(accepted > 100, "accept set suspiciously small: {accepted}");
    }

    /// Exhaustive over <= 4 segments from [`SEGS`] (plus a trailing NUL
    /// segment on <= 3): `validate_show_path` accepts exactly the non-empty, unrooted,
    /// NUL-free strings with no `..` segment — and everything it accepts is
    /// free of `..` / root components as `std::path::Path` sees it.
    #[test]
    fn exhaustive_show_path_le_4_segments() {
        let mut inputs = all_paths(4);
        inputs.extend(all_paths(3).into_iter().map(|s| format!("{s}/\0")));
        inputs.push(String::new());
        let mut accepted = 0;
        for s in inputs {
            let ok = validate_show_path(&s).is_ok();
            let spec = !s.is_empty()
                && !s.starts_with('/')
                && !s.contains('\0')
                && !s.split('/').any(|seg| seg == "..");
            assert_eq!(ok, spec, "{s:?}");
            if ok {
                accepted += 1;
                assert!(
                    Path::new(&s)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
                    "{s:?}"
                );
            }
        }
        assert!(accepted > 100, "accept set suspiciously small: {accepted}");
    }

    /// Exhaustive over all pairs of <= 3-segment paths from [`SEGS`] minus
    /// `.`/`..` (which repo paths never contain): `path_is_under` agrees with
    /// std's component-wise `Path::starts_with` (for a non-empty dir).
    #[test]
    fn exhaustive_path_is_under_le_3_segments() {
        let domain: Vec<String> = all_paths(3)
            .into_iter()
            .filter(|s| !s.starts_with('/'))
            .filter(|s| !s.split('/').any(|seg| seg == "." || seg == ".."))
            .collect();
        let mut hits = 0;
        for p in &domain {
            for d in &domain {
                let got = path_is_under(p, d);
                let dir_nonempty = Path::new(d).components().next().is_some();
                let spec = dir_nonempty && Path::new(p).starts_with(Path::new(d));
                assert_eq!(got, spec, "path={p:?} dir={d:?}");
                hits += usize::from(got);
            }
        }
        assert!(hits > 50, "match set suspiciously small: {hits}");
        assert!(path_is_under("a/x", "a"));
        assert!(path_is_under("a", "a/"));
        assert!(!path_is_under("ab", "a"));
        assert!(!path_is_under("ab/x", "a"));
        assert!(!path_is_under("a", ""));
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    fn any_comp() -> LexComp {
        match kani::any::<u8>() % 5 {
            0 => LexComp::Prefix,
            1 => LexComp::RootDir,
            2 => LexComp::CurDir,
            3 => LexComp::ParentDir,
            _ => LexComp::Normal,
        }
    }

    /// Containment over <= 6 symbolic components: an accepted component
    /// sequence, applied as a lexical join onto a base (root/prefix reset to
    /// outside, `..` pops, names push), never leaves the base and ends
    /// strictly below it; and a rejected one either contains an absolute /
    /// parent component or never names anything.
    #[kani::proof]
    #[kani::unwind(8)]
    fn contained_relative_is_strictly_inside_le_6_comps() {
        let comps: [LexComp; 6] = [
            any_comp(),
            any_comp(),
            any_comp(),
            any_comp(),
            any_comp(),
            any_comp(),
        ];
        let len: usize = kani::any();
        kani::assume(len <= 6);
        let comps = &comps[..len];

        let mut depth: i32 = 0;
        let mut escaped = false;
        let mut has_bad = false;
        let mut has_normal = false;
        for c in comps {
            match c {
                LexComp::Prefix | LexComp::RootDir => {
                    escaped = true;
                    has_bad = true;
                }
                LexComp::ParentDir => {
                    depth -= 1;
                    has_bad = true;
                    if depth < 0 {
                        escaped = true;
                    }
                }
                LexComp::CurDir => {}
                LexComp::Normal => {
                    depth += 1;
                    has_normal = true;
                }
            }
        }

        let ok = is_contained_relative(comps.iter().copied());
        if ok {
            assert!(!escaped);
            assert!(depth >= 1);
            assert!(!has_bad);
        } else {
            assert!(has_bad || !has_normal);
        }
    }

    const SHOW_ALPHABET: [u8; 4] = [b'a', b'.', b'/', 0];

    /// `validate_show_path_bytes` over every string of <= 7 bytes from
    /// `{a . / NUL}`: accepted iff non-empty, unrooted, NUL-free and with no
    /// `..` segment (spec computed by an independent segment scan).
    #[kani::proof]
    #[kani::unwind(10)]
    fn show_path_accepts_iff_no_traversal_le_7_bytes() {
        let mut buf = [0u8; 7];
        let mut i = 0;
        while i < 7 {
            let k: usize = kani::any();
            kani::assume(k < SHOW_ALPHABET.len());
            buf[i] = SHOW_ALPHABET[k];
            i += 1;
        }
        let len: usize = kani::any();
        kani::assume(len <= 7);
        let b = &buf[..len];

        // Independent spec: track the current segment as a tiny DFA
        // (0 = empty so far, 1 = ".", 2 = "..", 3 = anything else).
        let mut has_nul = false;
        let mut has_dotdot = false;
        let mut state = 0u8;
        let mut j = 0;
        while j <= len {
            if j == len || b[j] == b'/' {
                if state == 2 {
                    has_dotdot = true;
                }
                state = 0;
            } else {
                if b[j] == 0 {
                    has_nul = true;
                }
                state = match (state, b[j]) {
                    (0, b'.') => 1,
                    (1, b'.') => 2,
                    _ => 3,
                };
            }
            j += 1;
        }
        let spec = len > 0 && b[0] != b'/' && !has_nul && !has_dotdot;
        assert_eq!(validate_show_path_bytes(b).is_ok(), spec);
    }

    const UNDER_ALPHABET: [u8; 3] = [b'a', b'b', b'/'];

    fn any_path<const N: usize>(buf: &mut [u8; N]) -> usize {
        let mut i = 0;
        while i < N {
            let k: usize = kani::any();
            kani::assume(k < UNDER_ALPHABET.len());
            buf[i] = UNDER_ALPHABET[k];
            i += 1;
        }
        let len: usize = kani::any();
        kani::assume(len <= N);
        len
    }

    /// Split into its (at most 4) non-empty segments as (start, end) ranges.
    fn segments(b: &[u8]) -> ([(usize, usize); 4], usize) {
        let mut out = [(0usize, 0usize); 4];
        let mut n = 0;
        let mut start = 0;
        let mut i = 0;
        while i <= b.len() {
            if i == b.len() || b[i] == b'/' {
                if i > start {
                    out[n] = (start, i);
                    n += 1;
                }
                start = i + 1;
            }
            i += 1;
        }
        (out, n)
    }

    /// `path_bytes_under(p, d)` over all `p`, `d` of <= 6 bytes from
    /// `{a b /}` holds iff `d` has at least one segment and `d`'s non-empty
    /// segments are a prefix of `p`'s (so `a` covers `a/b` but not `ab`).
    #[kani::proof]
    #[kani::unwind(9)]
    fn path_under_is_segment_prefix_le_6_bytes() {
        let mut pb = [0u8; 6];
        let mut db = [0u8; 6];
        let pl = any_path(&mut pb);
        let dl = any_path(&mut db);
        let p = &pb[..pl];
        let d = &db[..dl];

        let (ps, pn) = segments(p);
        let (ds, dn) = segments(d);
        let mut spec = dn > 0 && dn <= pn;
        let mut s = 0;
        while spec && s < dn {
            let (a0, a1) = ds[s];
            let (b0, b1) = ps[s];
            if a1 - a0 != b1 - b0 {
                spec = false;
            } else {
                let mut k = 0;
                while k < a1 - a0 {
                    if d[a0 + k] != p[b0 + k] {
                        spec = false;
                    }
                    k += 1;
                }
            }
            s += 1;
        }
        assert_eq!(path_bytes_under(p, d), spec);
    }
}
