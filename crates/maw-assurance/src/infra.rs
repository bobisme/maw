//! Infrastructure-failure classification for the SG1 DST harness (bn-30v6e).
//!
//! The SG1 soak drives thousands of throwaway git repos through the in-proc
//! driver. When the *host* runs out of disk quota, disk space or file
//! descriptors, the driver panics or its oracles report a `GitError`. Before
//! this module those host failures were indistinguishable from Oracle A/B
//! violations: the 2026-07 soak campaign HALTED twice on `Disk quota exceeded`
//! with no oracle finding at all.
//!
//! This module gives the harness a **positive** classifier for exactly four
//! host conditions:
//!
//! | errno    | [`InfraKind`]                    | typical text                      |
//! |----------|----------------------------------|-----------------------------------|
//! | `EDQUOT` | [`InfraKind::QuotaExceeded`]     | `Disk quota exceeded`             |
//! | `ENOSPC` | [`InfraKind::StorageFull`]       | `No space left on device`         |
//! | `EMFILE` | [`InfraKind::TooManyOpenFiles`]  | `Too many open files`             |
//! | `ENFILE` | [`InfraKind::FileTableOverflow`] | `Too many open files in system`   |
//!
//! ## Fail-closed contract
//!
//! Classification is **opt-in by evidence**. Anything the classifier cannot
//! positively match returns `None`, and callers keep treating it as they did
//! before (a panic, an oracle violation, a red run). An infra classification
//! never counts a seed as clean — it aborts the run with
//! [`INFRA_EXIT_CODE`] so the soak neither accrues op-steps nor records a
//! violation for it.
//!
//! Oracle **findings** (`ReachabilityLost`, `DanglingHeadRef`, ...) are never
//! passed through this classifier. Only the oracles' own tooling-failure
//! variants (`GitError`) are, and only when their stderr matches.

use std::any::Any;
use std::fmt;
use std::io;

/// Process exit code the SG1 harness uses for an infrastructure failure.
///
/// `75` is `EX_TEMPFAIL` from `sysexits.h`: "temporary failure; the user is
/// invited to retry". libtest uses `101` for a failed test, so `75` never
/// collides with an ordinary test failure.
pub const INFRA_EXIT_CODE: i32 = 75;

/// Prefix of the single marker line the harness prints before exiting with
/// [`INFRA_EXIT_CODE`]. `scripts/sg1-soak/slot.sh` requires BOTH this marker
/// and the exit code before it treats a slot as infra.
pub const INFRA_MARKER: &str = "[sg1] INFRA-FAILURE:";

/// Which host resource ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InfraKind {
    /// `EDQUOT` — per-user disk quota exhausted.
    QuotaExceeded,
    /// `ENOSPC` — the filesystem is full.
    StorageFull,
    /// `EMFILE` — the per-process file-descriptor limit is reached.
    TooManyOpenFiles,
    /// `ENFILE` — the system-wide open-file table is full.
    FileTableOverflow,
}

impl InfraKind {
    /// Short stable identifier (the errno name).
    #[must_use]
    pub const fn errno_name(self) -> &'static str {
        match self {
            Self::QuotaExceeded => "EDQUOT",
            Self::StorageFull => "ENOSPC",
            Self::TooManyOpenFiles => "EMFILE",
            Self::FileTableOverflow => "ENFILE",
        }
    }
}

impl fmt::Display for InfraKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.errno_name())
    }
}

/// A positively classified infrastructure failure.
///
/// The in-proc driver raises this as a typed panic payload
/// ([`raise`]) so every caller that does not explicitly catch it still
/// fails (fail closed); the SG1 harness catches it and exits with
/// [`INFRA_EXIT_CODE`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfraFailure {
    /// Which resource ran out.
    pub kind: InfraKind,
    /// Human-readable context (the failing operation plus the error text).
    pub detail: String,
}

impl fmt::Display for InfraFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}

impl InfraFailure {
    /// Build the single marker line (`[sg1] INFRA-FAILURE: <reason>`).
    /// Newlines in the detail are flattened so the marker stays one line.
    #[must_use]
    pub fn marker_line(&self, context: &str) -> String {
        let flat: String = self
            .to_string()
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        if context.is_empty() {
            format!("{INFRA_MARKER} {flat}")
        } else {
            format!("{INFRA_MARKER} {flat} ({context})")
        }
    }
}

// errno values. EMFILE/ENFILE/ENOSPC are identical on Linux and the BSDs;
// EDQUOT differs, so it is taken per target.
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
const ENOSPC: i32 = 28;
#[cfg(target_os = "linux")]
const EDQUOT: i32 = 122;
#[cfg(not(target_os = "linux"))]
const EDQUOT: i32 = 69;

/// Classify an errno value.
#[must_use]
pub const fn classify_errno(code: i32) -> Option<InfraKind> {
    match code {
        EDQUOT => Some(InfraKind::QuotaExceeded),
        ENOSPC => Some(InfraKind::StorageFull),
        EMFILE => Some(InfraKind::TooManyOpenFiles),
        ENFILE => Some(InfraKind::FileTableOverflow),
        _ => None,
    }
}

/// Classify free-form error text (git stderr, a panic message, an
/// `io::Error` rendered with `Display`/`Debug`).
///
/// Matches the exact strerror(3) texts glibc/musl print for the four errnos.
/// Everything else is `None`.
#[must_use]
pub fn classify_text(text: &str) -> Option<InfraKind> {
    // ENFILE's text is a superstring of EMFILE's, so test it first.
    if text.contains("Too many open files in system") {
        Some(InfraKind::FileTableOverflow)
    } else if text.contains("Disk quota exceeded") {
        Some(InfraKind::QuotaExceeded)
    } else if text.contains("No space left on device") {
        Some(InfraKind::StorageFull)
    } else if text.contains("Too many open files") {
        Some(InfraKind::TooManyOpenFiles)
    } else {
        None
    }
}

/// Classify an `io::Error`: its raw OS code, its `ErrorKind`, then its text
/// (the in-proc driver wraps git stderr in `io::Error::other`).
#[must_use]
pub fn classify_io_error(err: &io::Error) -> Option<InfraKind> {
    if let Some(kind) = err.raw_os_error().and_then(classify_errno) {
        return Some(kind);
    }
    match err.kind() {
        io::ErrorKind::QuotaExceeded => return Some(InfraKind::QuotaExceeded),
        io::ErrorKind::StorageFull => return Some(InfraKind::StorageFull),
        _ => {}
    }
    classify_text(&err.to_string())
}

/// Classify a panic payload caught with `std::panic::catch_unwind`.
///
/// A typed [`InfraFailure`] payload (from [`raise`]) is returned as is. A
/// string payload (`panic!`/`expect`) is classified by its text. Any other
/// payload is `None`.
#[must_use]
pub fn classify_panic_payload(payload: &(dyn Any + Send)) -> Option<InfraFailure> {
    if let Some(f) = payload.downcast_ref::<InfraFailure>() {
        return Some(f.clone());
    }
    let text = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())?;
    classify_text(text).map(|kind| InfraFailure {
        kind,
        detail: text.to_owned(),
    })
}

/// Abort the current seed with a typed [`InfraFailure`] panic payload.
///
/// Callers that do not catch it fail as they would on any panic.
pub fn raise(failure: InfraFailure) -> ! {
    // The default panic hook prints a typed payload as `Box<dyn Any>`;
    // say what it is first so logs stay readable.
    eprintln!("[infra] host resource failure: {failure}");
    std::panic::panic_any(failure)
}

/// If `err` is infra, [`raise`] it with `context`; otherwise return.
pub fn raise_if_infra_io(err: &io::Error, context: &str) {
    if let Some(kind) = classify_io_error(err) {
        raise(InfraFailure {
            kind,
            detail: format!("{context}: {err}"),
        });
    }
}

/// If `text` is infra, [`raise`] it with `context`; otherwise return.
pub fn raise_if_infra_text(text: &str, context: &str) {
    if let Some(kind) = classify_text(text) {
        raise(InfraFailure {
            kind,
            detail: format!("{context}: {}", text.trim()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_codes_classify() {
        for (code, kind) in [
            (EDQUOT, InfraKind::QuotaExceeded),
            (ENOSPC, InfraKind::StorageFull),
            (EMFILE, InfraKind::TooManyOpenFiles),
            (ENFILE, InfraKind::FileTableOverflow),
        ] {
            let err = io::Error::from_raw_os_error(code);
            assert_eq!(classify_io_error(&err), Some(kind), "errno {code}: {err}");
        }
    }

    #[test]
    fn error_kinds_classify() {
        assert_eq!(
            classify_io_error(&io::Error::from(io::ErrorKind::QuotaExceeded)),
            Some(InfraKind::QuotaExceeded)
        );
        assert_eq!(
            classify_io_error(&io::Error::from(io::ErrorKind::StorageFull)),
            Some(InfraKind::StorageFull)
        );
    }

    /// The two stderr texts that halted the pre.6 soak campaign, verbatim
    /// in shape.
    #[test]
    fn git_stderr_from_the_halted_campaign_classifies() {
        let init = io::Error::other(
            "git init -q -b main failed: fatal: cannot mkdir /tmp/.tmpAbC/.git: \
             Disk quota exceeded\n",
        );
        assert_eq!(classify_io_error(&init), Some(InfraKind::QuotaExceeded));
        let hash = "git hash-object -w --stdin failed: error: unable to create temporary \
                    file: Disk quota exceeded\nfatal: unable to add stdin to database";
        assert_eq!(classify_text(hash), Some(InfraKind::QuotaExceeded));
        assert_eq!(
            classify_text("fatal: write error: No space left on device"),
            Some(InfraKind::StorageFull)
        );
        assert_eq!(
            classify_text(
                "git spawn: Os { code: 24, kind: Uncategorized, message: \"Too many open files\" }"
            ),
            Some(InfraKind::TooManyOpenFiles)
        );
        assert_eq!(
            classify_text("Too many open files in system"),
            Some(InfraKind::FileTableOverflow)
        );
    }

    /// Fail closed: ordinary errors and oracle findings never classify.
    #[test]
    fn non_infra_errors_do_not_classify() {
        for code in [1, 2, 5, 13, 17, 20, 21, 30, 110] {
            // EPERM ENOENT EIO EACCES EEXIST ENOTDIR EISDIR EROFS ETIMEDOUT
            let err = io::Error::from_raw_os_error(code);
            assert_eq!(classify_io_error(&err), None, "errno {code}: {err}");
        }
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(classify_io_error(&io::Error::from(kind)), None, "{kind:?}");
        }
        for text in [
            "",
            "fatal: bad object deadbeef",
            "git update-ref failed: fatal: cannot lock ref 'refs/heads/main'",
            "SG1 nightly soak FAILED (release-blocking; §7 acceptance gate):\n  - seed=7 \
             verdict=OracleA(OracleAClass { kind: \"ReachabilityLost\", oid: \"abc\" })",
            "B1 violation: dangling refs/manifold/head/ws-0 -> abc",
            "quota", // a bare word is not evidence
            "open files",
        ] {
            assert_eq!(classify_text(text), None, "{text:?}");
        }
    }

    #[test]
    fn panic_payloads_classify_both_directions() {
        let typed: Box<dyn Any + Send> = Box::new(InfraFailure {
            kind: InfraKind::StorageFull,
            detail: "x".into(),
        });
        assert_eq!(
            classify_panic_payload(typed.as_ref()).map(|f| f.kind),
            Some(InfraKind::StorageFull)
        );
        let s: Box<dyn Any + Send> =
            Box::new(String::from("in-proc driver init: ... Disk quota exceeded"));
        assert_eq!(
            classify_panic_payload(s.as_ref()).map(|f| f.kind),
            Some(InfraKind::QuotaExceeded)
        );
        let st: Box<dyn Any + Send> = Box::new("No space left on device");
        assert!(classify_panic_payload(st.as_ref()).is_some());

        let real: Box<dyn Any + Send> = Box::new(String::from("assertion failed: w ⊆ U(F)"));
        assert_eq!(classify_panic_payload(real.as_ref()), None);
        let other: Box<dyn Any + Send> = Box::new(42_u32);
        assert_eq!(classify_panic_payload(other.as_ref()), None);
    }

    #[test]
    fn raise_carries_a_typed_payload() {
        let caught = std::panic::catch_unwind(|| {
            raise_if_infra_io(&io::Error::from_raw_os_error(EDQUOT), "git init");
        })
        .expect_err("raise must panic");
        let f = classify_panic_payload(caught.as_ref()).expect("typed payload");
        assert_eq!(f.kind, InfraKind::QuotaExceeded);
        assert!(f.detail.starts_with("git init: "), "{}", f.detail);

        // Non-infra errors do not raise.
        raise_if_infra_io(&io::Error::from(io::ErrorKind::NotFound), "x");
        raise_if_infra_text("fatal: bad object", "x");
    }

    #[test]
    fn marker_line_is_one_line() {
        let f = InfraFailure {
            kind: InfraKind::QuotaExceeded,
            detail: "git init failed:\nDisk quota exceeded\n".into(),
        };
        let line = f.marker_line("seed=5");
        assert!(line.starts_with(INFRA_MARKER));
        assert!(!line.contains('\n'));
        assert!(line.contains("EDQUOT"));
        assert!(line.ends_with("(seed=5)"));
    }
}
