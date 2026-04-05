//! Credential resolution for LFS HTTPS transfers.
//!
//! Resolution order (first match wins):
//!
//! 1. In-memory cache (populated on successful lookups).
//! 2. Environment variables: `MAW_LFS_USERNAME` / `MAW_LFS_PASSWORD`
//!    (applied to ALL hosts — use for CI / single-host setups).
//! 3. `~/.netrc` file (standard machine/login/password format).
//!
//! No subprocess is spawned. Full in-process resolution.
//!
//! # Future
//!
//! A `gix-credentials` integration for spawning git credential helpers is
//! planned but not MVP. Users who need helper integration can run
//! `git credential store` once to populate `~/.git-credentials`, which the
//! netrc parser does not read; for now, populate `~/.netrc` with the LFS
//! server host.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct BasicCreds {
    pub username: String,
    pub password: String,
}

pub struct CredentialProvider {
    cache: HashMap<String, BasicCreds>,
    env: Option<BasicCreds>,
    netrc_entries: Vec<(String, BasicCreds)>, // (host, creds)
}

#[derive(Debug, Error)]
pub enum CredsError {
    #[error("no credentials available for {host}")]
    Missing { host: String },
    #[error("netrc parse error at line {line}: {message}")]
    NetrcParse { line: usize, message: String },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Default for CredentialProvider {
    fn default() -> Self {
        Self::empty()
    }
}

impl CredentialProvider {
    /// An empty provider that has no credentials for any host.
    pub fn empty() -> Self {
        Self {
            cache: HashMap::new(),
            env: None,
            netrc_entries: Vec::new(),
        }
    }

    /// Build a provider from the standard sources (env + netrc).
    pub fn from_env_and_netrc() -> Result<Self, CredsError> {
        let env = match (
            std::env::var("MAW_LFS_USERNAME").ok(),
            std::env::var("MAW_LFS_PASSWORD").ok(),
        ) {
            (Some(u), Some(p)) => Some(BasicCreds {
                username: u,
                password: p,
            }),
            _ => None,
        };
        let netrc_entries = load_netrc().unwrap_or_default();
        Ok(Self {
            cache: HashMap::new(),
            env,
            netrc_entries,
        })
    }

    /// Insert credentials explicitly (for testing or programmatic config).
    pub fn insert(&mut self, host: &str, creds: BasicCreds) {
        self.cache.insert(host.to_owned(), creds);
    }

    /// Resolve credentials for `host`, or return Missing.
    pub fn get(&mut self, host: &str) -> Result<BasicCreds, CredsError> {
        if let Some(c) = self.cache.get(host) {
            return Ok(c.clone());
        }
        if let Some(c) = &self.env {
            self.cache.insert(host.to_owned(), c.clone());
            return Ok(c.clone());
        }
        for (h, c) in &self.netrc_entries {
            if h == host {
                self.cache.insert(host.to_owned(), c.clone());
                return Ok(c.clone());
            }
        }
        Err(CredsError::Missing {
            host: host.to_owned(),
        })
    }

    /// Evict cached credentials for `host` (call on 401/403 response).
    pub fn reject(&mut self, host: &str) {
        self.cache.remove(host);
    }
}

fn load_netrc() -> Option<Vec<(String, BasicCreds)>> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path = home.join(".netrc");
    let text = fs::read_to_string(&path).ok()?;
    parse_netrc(&text).ok()
}

fn parse_netrc(text: &str) -> Result<Vec<(String, BasicCreds)>, CredsError> {
    // Very minimal netrc parser: handles machine/login/password tokens,
    // ignores 'default' / 'account' / 'macdef' blocks. Tokens are
    // whitespace-separated; all on separate lines or same line.
    let mut out = Vec::new();
    let mut tokens = text.split_whitespace().peekable();
    let mut cur_machine: Option<String> = None;
    let mut cur_login: Option<String> = None;
    let mut cur_password: Option<String> = None;

    fn flush(
        machine: &mut Option<String>,
        login: &mut Option<String>,
        password: &mut Option<String>,
        out: &mut Vec<(String, BasicCreds)>,
    ) {
        if let (Some(m), Some(l), Some(p)) = (machine.take(), login.take(), password.take()) {
            out.push((
                m,
                BasicCreds {
                    username: l,
                    password: p,
                },
            ));
        } else {
            machine.take();
            login.take();
            password.take();
        }
    }

    while let Some(tok) = tokens.next() {
        match tok {
            "machine" => {
                flush(&mut cur_machine, &mut cur_login, &mut cur_password, &mut out);
                cur_machine = tokens.next().map(|s| s.to_owned());
            }
            "default" => {
                flush(&mut cur_machine, &mut cur_login, &mut cur_password, &mut out);
                cur_machine = Some("".to_owned()); // sentinel for default
            }
            "login" => cur_login = tokens.next().map(|s| s.to_owned()),
            "password" => cur_password = tokens.next().map(|s| s.to_owned()),
            "account" => {
                let _ = tokens.next();
            }
            "macdef" => {
                // Skip until blank line — our simplified parser drops the block.
                let _ = tokens.next();
            }
            _ => {} // unknown tokens ignored
        }
    }
    flush(&mut cur_machine, &mut cur_login, &mut cur_password, &mut out);
    // Remove default-sentinel (we don't apply default-credentials to arbitrary hosts).
    out.retain(|(h, _)| !h.is_empty());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_provider_has_no_creds() {
        let mut p = CredentialProvider::empty();
        assert!(matches!(p.get("example.com"), Err(CredsError::Missing { .. })));
    }

    #[test]
    fn insert_and_get() {
        let mut p = CredentialProvider::empty();
        p.insert(
            "github.com",
            BasicCreds {
                username: "alice".to_owned(),
                password: "token".to_owned(),
            },
        );
        let c = p.get("github.com").unwrap();
        assert_eq!(c.username, "alice");
        assert_eq!(c.password, "token");
    }

    #[test]
    fn reject_evicts_cache() {
        let mut p = CredentialProvider::empty();
        p.insert(
            "a.example",
            BasicCreds {
                username: "u".to_owned(),
                password: "p".to_owned(),
            },
        );
        assert!(p.get("a.example").is_ok());
        p.reject("a.example");
        assert!(matches!(
            p.get("a.example"),
            Err(CredsError::Missing { .. })
        ));
    }

    #[test]
    fn parse_netrc_basic() {
        let text = "\
machine github.com
login alice
password ghp_abc123

machine gitlab.example.com login bob password xyz
";
        let entries = parse_netrc(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "github.com");
        assert_eq!(entries[0].1.username, "alice");
        assert_eq!(entries[0].1.password, "ghp_abc123");
        assert_eq!(entries[1].0, "gitlab.example.com");
        assert_eq!(entries[1].1.username, "bob");
        assert_eq!(entries[1].1.password, "xyz");
    }

    #[test]
    fn parse_netrc_skips_incomplete_entries() {
        let text = "machine incomplete.example login onlyuser\n\
                    machine good.example login u password p\n";
        let entries = parse_netrc(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "good.example");
    }

    #[test]
    fn parse_netrc_default_block_not_applied() {
        let text = "default login anyone password anypass\n";
        let entries = parse_netrc(text).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_netrc_ignores_account() {
        let text = "machine x login u account acct password p\n";
        let entries = parse_netrc(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.username, "u");
        assert_eq!(entries[0].1.password, "p");
    }
}
