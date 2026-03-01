//! Audit event logging for recovery operations.
//!
//! Emits structured JSON lines to stderr whenever a security-relevant
//! recovery operation is performed (search, show, restore, prune).
//! This supports the security model described in the assurance plan (section 13).
//!
//! # Security constraints
//!
//! - Search patterns are stored as SHA-256 hashes, NEVER as plaintext
//!   (they may match secrets in the recovered content).
//! - No raw file-content snippets appear in audit output.
//! - Audit output goes to stderr so it doesn't interfere with structured
//!   stdout output consumed by agents.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// A structured audit event for recovery operations.
#[derive(Debug, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A content search across recovery snapshots.
    Search {
        /// SHA-256 hash of the raw search pattern (never log the pattern itself).
        pattern_hash: String,
        /// Workspace filter, if any.
        workspace_filter: Option<String>,
        /// Recovery ref filter, if any.
        ref_filter: Option<String>,
        /// Number of search hits returned.
        hit_count: usize,
    },
    /// Viewing a specific file from a recovery snapshot.
    Show {
        /// The recovery ref name used.
        ref_name: String,
        /// The file path requested.
        path: String,
    },
    /// Restoring a recovery snapshot into a new workspace.
    Restore {
        /// The recovery ref or workspace name being restored from.
        ref_name: String,
        /// The name of the new workspace created.
        new_workspace: String,
    },
    /// Pruning recovery refs and/or artifacts.
    #[allow(dead_code)]
    Prune {
        /// Number of refs removed.
        refs_removed: usize,
        /// Number of artifacts removed.
        artifacts_removed: usize,
    },
}

/// Write a structured JSON audit line to stderr.
///
/// Uses `eprintln!` so it appears on stderr without interfering with
/// stdout output. The `[AUDIT]` prefix makes it easy to filter in logs.
pub fn log_audit(event: &AuditEvent) {
    let json = serde_json::to_string(event).unwrap_or_default();
    eprintln!("[AUDIT] {json}");
}

/// Hash a search pattern with SHA-256.
///
/// Returns the lowercase hex digest. This is used to log search patterns
/// without revealing their plaintext (which may match secrets).
pub fn hash_pattern(pattern: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(pattern.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_pattern_is_consistent() {
        let h1 = hash_pattern("secret-token-123");
        let h2 = hash_pattern("secret-token-123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_pattern_produces_valid_sha256() {
        let h = hash_pattern("hello");
        // SHA-256 hex digest is always 64 hex characters.
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_pattern_known_value() {
        // echo -n "hello" | sha256sum
        let h = hash_pattern("hello");
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn search_event_serialization_contains_expected_fields() {
        let event = AuditEvent::Search {
            pattern_hash: hash_pattern("needle"),
            workspace_filter: Some("alice".to_string()),
            ref_filter: None,
            hit_count: 3,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"search\""));
        assert!(json.contains("\"pattern_hash\""));
        assert!(json.contains("\"hit_count\":3"));
        assert!(json.contains("\"workspace_filter\":\"alice\""));
        assert!(json.contains("\"ref_filter\":null"));
    }

    #[test]
    fn search_event_does_not_contain_raw_pattern() {
        let raw_pattern = "my-super-secret-api-key-12345";
        let event = AuditEvent::Search {
            pattern_hash: hash_pattern(raw_pattern),
            workspace_filter: None,
            ref_filter: None,
            hit_count: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains(raw_pattern),
            "audit JSON must never contain the raw search pattern"
        );
    }

    #[test]
    fn show_event_serialization() {
        let event = AuditEvent::Show {
            ref_name: "refs/manifold/recovery/alice/2025-01-01".to_string(),
            path: "src/main.rs".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"show\""));
        assert!(json.contains("\"ref_name\""));
        assert!(json.contains("\"path\":\"src/main.rs\""));
    }

    #[test]
    fn restore_event_serialization() {
        let event = AuditEvent::Restore {
            ref_name: "refs/manifold/recovery/alice/2025-01-01".to_string(),
            new_workspace: "recovered-alice".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"restore\""));
        assert!(json.contains("\"new_workspace\":\"recovered-alice\""));
    }

    #[test]
    fn prune_event_serialization() {
        let event = AuditEvent::Prune {
            refs_removed: 5,
            artifacts_removed: 2,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"prune\""));
        assert!(json.contains("\"refs_removed\":5"));
        assert!(json.contains("\"artifacts_removed\":2"));
    }
}
