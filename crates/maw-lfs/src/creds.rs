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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct BasicCreds {
    pub username: String,
    pub password: String,
}

pub struct CredentialProvider {
    cache: HashMap<String, CachedCreds>,
    env: Option<BasicCreds>,
    netrc_entries: Vec<(String, BasicCreds)>, // (host, creds)
    rejected: HashMap<String, HashSet<CredentialSource>>,
}

#[derive(Debug, Clone)]
struct CachedCreds {
    creds: BasicCreds,
    source: CredentialSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CredentialSource {
    Explicit,
    Env,
    Netrc(usize),
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
    #[must_use]
    pub fn empty() -> Self {
        Self {
            cache: HashMap::new(),
            env: None,
            netrc_entries: Vec::new(),
            rejected: HashMap::new(),
        }
    }

    /// Build a provider from the standard sources (env + netrc).
    ///
    /// # Errors
    /// Returns an error if a credential source cannot be parsed.
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
            rejected: HashMap::new(),
        })
    }

    #[cfg(test)]
    fn from_sources(env: Option<BasicCreds>, netrc_entries: Vec<(String, BasicCreds)>) -> Self {
        Self {
            cache: HashMap::new(),
            env,
            netrc_entries,
            rejected: HashMap::new(),
        }
    }

    /// Insert credentials explicitly (for testing or programmatic config).
    pub fn insert(&mut self, host: &str, creds: BasicCreds) {
        self.cache.insert(
            host.to_owned(),
            CachedCreds {
                creds,
                source: CredentialSource::Explicit,
            },
        );
    }

    /// Resolve credentials for `host`, or return Missing.
    ///
    /// # Errors
    /// Returns [`CredsError::Missing`] if no non-rejected credentials are
    /// available for `host`.
    pub fn get(&mut self, host: &str) -> Result<BasicCreds, CredsError> {
        if let Some(c) = self.cache.get(host) {
            return Ok(c.creds.clone());
        }

        let rejected = self.rejected.get(host);
        if let Some(c) = self
            .env
            .as_ref()
            .filter(|_| rejected.is_none_or(|r| !r.contains(&CredentialSource::Env)))
        {
            self.cache.insert(
                host.to_owned(),
                CachedCreds {
                    creds: c.clone(),
                    source: CredentialSource::Env,
                },
            );
            return Ok(c.clone());
        }

        for (idx, (h, c)) in self.netrc_entries.iter().enumerate() {
            let source = CredentialSource::Netrc(idx);
            if h == host && rejected.is_none_or(|r| !r.contains(&source)) {
                self.cache.insert(
                    host.to_owned(),
                    CachedCreds {
                        creds: c.clone(),
                        source,
                    },
                );
                return Ok(c.clone());
            }
        }
        Err(CredsError::Missing {
            host: host.to_owned(),
        })
    }

    /// Evict cached credentials for `host` (call on 401/403 response).
    pub fn reject(&mut self, host: &str) {
        if let Some(cached) = self.cache.remove(host) {
            self.rejected
                .entry(host.to_owned())
                .or_default()
                .insert(cached.source);
        }
    }
}

fn load_netrc() -> Option<Vec<(String, BasicCreds)>> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path = home.join(".netrc");
    let text = fs::read_to_string(&path).ok()?;
    parse_netrc(&text).ok()
}

fn flush_netrc_entry(
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

fn parse_netrc(text: &str) -> Result<Vec<(String, BasicCreds)>, CredsError> {
    // Very minimal netrc parser: handles machine/login/password tokens,
    // ignores 'default' / 'account' / 'macdef' blocks. Tokens are
    // whitespace-separated; all on separate lines or same line.
    let mut out = Vec::new();
    let mut tokens = text.split_whitespace();
    let mut cur_machine: Option<String> = None;
    let mut cur_login: Option<String> = None;
    let mut cur_password: Option<String> = None;

    while let Some(tok) = tokens.next() {
        match tok {
            "machine" => {
                flush_netrc_entry(
                    &mut cur_machine,
                    &mut cur_login,
                    &mut cur_password,
                    &mut out,
                );
                cur_machine = tokens.next().map(std::borrow::ToOwned::to_owned);
            }
            "default" => {
                flush_netrc_entry(
                    &mut cur_machine,
                    &mut cur_login,
                    &mut cur_password,
                    &mut out,
                );
                cur_machine = Some(String::new()); // sentinel for default
            }
            "login" => cur_login = tokens.next().map(std::borrow::ToOwned::to_owned),
            "password" => cur_password = tokens.next().map(std::borrow::ToOwned::to_owned),
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
    flush_netrc_entry(
        &mut cur_machine,
        &mut cur_login,
        &mut cur_password,
        &mut out,
    );
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
        assert!(matches!(
            p.get("example.com"),
            Err(CredsError::Missing { .. })
        ));
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
        let c = p.get("github.com").expect("operation should succeed");
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
    fn reject_env_credentials_falls_back_to_netrc() {
        let mut p = CredentialProvider::from_sources(
            Some(BasicCreds {
                username: "bad-env-user".to_owned(),
                password: "bad-env-token".to_owned(),
            }),
            vec![(
                "github.com".to_owned(),
                BasicCreds {
                    username: "netrc-user".to_owned(),
                    password: "netrc-token".to_owned(),
                },
            )],
        );

        let first = p.get("github.com").expect("operation should succeed");
        assert_eq!(first.username, "bad-env-user");

        p.reject("github.com");
        let second = p.get("github.com").expect("operation should succeed");
        assert_eq!(second.username, "netrc-user");
        assert_eq!(second.password, "netrc-token");
    }

    #[test]
    fn reject_netrc_credentials_does_not_reuse_same_entry() {
        let mut p = CredentialProvider::from_sources(
            None,
            vec![(
                "github.com".to_owned(),
                BasicCreds {
                    username: "bad-netrc-user".to_owned(),
                    password: "bad-netrc-token".to_owned(),
                },
            )],
        );

        let first = p.get("github.com").expect("operation should succeed");
        assert_eq!(first.username, "bad-netrc-user");

        p.reject("github.com");
        assert!(matches!(
            p.get("github.com"),
            Err(CredsError::Missing { .. })
        ));
    }

    #[test]
    fn reject_first_netrc_entry_falls_back_to_next_matching_entry() {
        let mut p = CredentialProvider::from_sources(
            None,
            vec![
                (
                    "github.com".to_owned(),
                    BasicCreds {
                        username: "old-user".to_owned(),
                        password: "old-token".to_owned(),
                    },
                ),
                (
                    "github.com".to_owned(),
                    BasicCreds {
                        username: "new-user".to_owned(),
                        password: "new-token".to_owned(),
                    },
                ),
            ],
        );

        let first = p.get("github.com").expect("operation should succeed");
        assert_eq!(first.username, "old-user");

        p.reject("github.com");
        let second = p.get("github.com").expect("operation should succeed");
        assert_eq!(second.username, "new-user");
        assert_eq!(second.password, "new-token");
    }

    #[test]
    fn parse_netrc_basic() {
        let text = "\
machine github.com
login alice
password ghp_abc123

machine gitlab.example.com login bob password xyz
";
        let entries = parse_netrc(text).expect("operation should succeed");
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
        let entries = parse_netrc(text).expect("operation should succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "good.example");
    }

    #[test]
    fn parse_netrc_default_block_not_applied() {
        let text = "default login anyone password anypass\n";
        let entries = parse_netrc(text).expect("operation should succeed");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_netrc_ignores_account() {
        let text = "machine x login u account acct password p\n";
        let entries = parse_netrc(text).expect("operation should succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.username, "u");
        assert_eq!(entries[0].1.password, "p");
    }
}
