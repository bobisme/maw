//! Unknown-key detection for `.maw.toml` (bn-1losw).
//!
//! `MawConfig` does not use `deny_unknown_fields`: a config written for a
//! newer maw must keep working on an older one. But then a misspelled key
//! (`brnach = "trunk"`) is valid TOML that serde silently ignores, so maw runs
//! on the default instead. This module finds those keys so the loader can warn,
//! naming the key path and the nearest valid key.

/// Every key `MawConfig` reads, by section. Keep in sync with the
/// `MawConfig` / `*Config` structs in `workspace/mod.rs`; a key missing here
/// only causes a spurious warning, never a behavior change.
///
/// `[merge]` sub-keys are checked by maw-core's layered loader
/// (`ManifoldConfig::load_layered`, which warns about `[merge]` keys it
/// ignores), so they are not re-checked here.
const KNOWN_KEYS: &[(&str, Option<&[&str]>)] = &[
    ("repo", Some(&["branch", "default_workspace"])),
    ("lock", Some(&["no_wait", "wait_seconds"])),
    ("invariant", Some(&["audit"])),
    (
        "hooks",
        Some(&[
            "pre_merge",
            "post_merge",
            "post_sync",
            "hook_timeout_seconds",
        ]),
    ),
    ("merge", None),
];

/// An unknown key in `.maw.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnknownKey {
    /// Dotted key path, e.g. `repo.brnach`.
    pub path: String,
    /// The nearest valid key path, when one is close enough to suggest.
    pub nearest: Option<String>,
}

impl UnknownKey {
    /// Human-readable warning line (without the file prefix).
    pub(super) fn describe(&self) -> String {
        let hint = self
            .nearest
            .as_ref()
            .map_or_else(String::new, |n| format!(" (did you mean `{n}`?)"));
        format!("unknown key `{}` is ignored{hint}", self.path)
    }
}

/// All known dotted key paths, including bare section names.
fn known_paths() -> Vec<String> {
    let mut out = Vec::new();
    for (section, keys) in KNOWN_KEYS {
        out.push((*section).to_owned());
        for key in keys.unwrap_or(&[]) {
            out.push(format!("{section}.{key}"));
        }
    }
    out
}

/// Find keys in a parsed `.maw.toml` that `MawConfig` does not read.
///
/// `read_elsewhere` names top-level sections another loader reads from this
/// same file (the consolidated layout's deprecated `.maw/config.toml`); they
/// are skipped rather than reported as ignored.
pub(super) fn unknown_keys(table: &toml::Table, read_elsewhere: &[&str]) -> Vec<UnknownKey> {
    let mut out = Vec::new();
    for (key, value) in table {
        if read_elsewhere.contains(&key.as_str()) {
            continue;
        }
        match KNOWN_KEYS.iter().find(|(section, _)| section == key) {
            None => out.push(UnknownKey {
                path: key.clone(),
                nearest: nearest(key),
            }),
            Some((_, None)) => {}
            Some((section, Some(keys))) => {
                let Some(sub) = value.as_table() else {
                    // A non-table section is a type error serde already reported.
                    continue;
                };
                for sub_key in sub.keys() {
                    if !keys.contains(&sub_key.as_str()) {
                        let path = format!("{section}.{sub_key}");
                        let nearest = nearest(&path);
                        out.push(UnknownKey { path, nearest });
                    }
                }
            }
        }
    }
    out
}

/// The nearest valid key path to `path`: first a known key with the same leaf
/// name in another section (`[repo] wait_seconds` -> `lock.wait_seconds`,
/// top-level `branch` -> `repo.branch`), else the closest by edit distance
/// within a small threshold.
fn nearest(path: &str) -> Option<String> {
    let known = known_paths();
    let leaf = path.rsplit('.').next().unwrap_or(path);
    if let Some(same_leaf) = known
        .iter()
        .find(|k| k.as_str() != path && k.rsplit('.').next() == Some(leaf))
    {
        return Some(same_leaf.clone());
    }
    let threshold = (path.chars().count() / 3).max(2);
    known
        .into_iter()
        .map(|k| (levenshtein(path, &k), k))
        .filter(|(d, _)| *d <= threshold)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Plain Levenshtein edit distance over chars.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(src: &str) -> Vec<UnknownKey> {
        unknown_keys(&toml::from_str(src).expect("valid toml"), &[])
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("brnach", "branch"), 2);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn known_keys_are_not_reported() {
        let src = "[repo]\nbranch='m'\ndefault_workspace='d'\n[lock]\nno_wait=true\n\
                   wait_seconds=1\n[invariant]\naudit=false\n[hooks]\npre_merge=[]\n\
                   post_merge=[]\npost_sync=[]\nhook_timeout_seconds=1\n\
                   [merge]\nanything=1\n";
        assert_eq!(unknown(src), vec![]);
    }

    #[test]
    fn typos_get_nearest_key() {
        let got = unknown("[repo]\nbrnach='t'\n[lokc]\nno_wait=true\n");
        assert_eq!(
            got,
            vec![
                UnknownKey {
                    path: "lokc".into(),
                    nearest: Some("lock".into())
                },
                UnknownKey {
                    path: "repo.brnach".into(),
                    nearest: Some("repo.branch".into())
                },
            ]
        );
    }

    #[test]
    fn misplaced_key_suggests_its_real_section() {
        let got = unknown("branch='t'\n[repo]\nwait_seconds=3\n");
        assert_eq!(got[0].nearest.as_deref(), Some("repo.branch"));
        assert_eq!(got[1].nearest.as_deref(), Some("lock.wait_seconds"));
    }

    #[test]
    fn far_off_keys_get_no_suggestion() {
        let got = unknown("[telemetry]\nenabled=true\n");
        assert_eq!(
            got,
            vec![UnknownKey {
                path: "telemetry".into(),
                nearest: None
            }]
        );
    }
}
