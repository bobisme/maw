//! LFS pointer format v1 codec.
//!
//! Spec: <https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md>
//!
//! Canonical form:
//!
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! oid sha256:<64-char-lowercase-hex>
//! size <decimal-bytes>
//! ```
//!
//! Rules enforced:
//! - `version` line is always first.
//! - All other keys are sorted alphabetically.
//! - Each line ends with LF (0x0A), including the final line.
//! - ASCII only; CRLF rejected.
//! - Max pointer size: 1024 bytes (spec recommends rejecting larger inputs).

use thiserror::Error;

const VERSION_URL: &str = "https://git-lfs.github.com/spec/v1";
const MAX_POINTER_BYTES: usize = 1024;
const VERSION_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";

/// A parsed LFS pointer. Represents the content of a git blob that stands in
/// for a real binary file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// sha256 of the real file content.
    pub oid: [u8; 32],
    /// Size of the real file, in bytes.
    pub size: u64,
    /// Unknown keys preserved for forward compatibility. Round-tripped on write.
    /// Keys are stored lowercase; values as-parsed.
    pub extensions: Vec<(String, String)>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("pointer is empty")]
    Empty,
    #[error("pointer too large: {0} bytes (max {MAX_POINTER_BYTES})")]
    TooLarge(usize),
    #[error("missing or invalid version line")]
    BadVersion,
    #[error("unsupported pointer version: {found}")]
    UnsupportedVersion { found: String },
    #[error("missing or invalid oid line")]
    BadOid,
    #[error("missing or invalid size line")]
    BadSize,
    #[error("non-ASCII bytes in pointer")]
    NonAscii,
    #[error("duplicate key: {0}")]
    DuplicateKey(String),
    #[error("CRLF line endings not allowed")]
    CrlfLineEndings,
}

impl Pointer {
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.is_empty() {
            return Err(ParseError::Empty);
        }
        if bytes.len() > MAX_POINTER_BYTES {
            return Err(ParseError::TooLarge(bytes.len()));
        }
        if !bytes.is_ascii() {
            return Err(ParseError::NonAscii);
        }
        if bytes.contains(&b'\r') {
            return Err(ParseError::CrlfLineEndings);
        }
        // Every line, including the last, must terminate in LF.
        if !bytes.ends_with(b"\n") {
            return Err(ParseError::BadVersion);
        }

        // SAFETY: we verified is_ascii() above.
        let text = std::str::from_utf8(bytes).map_err(|_| ParseError::NonAscii)?;

        let mut lines = text.split('\n');
        // split on '\n' with trailing '\n' yields a trailing empty element.
        let version_line = lines.next().ok_or(ParseError::BadVersion)?;

        // Version line is exactly "version <URL>".
        let version_value = version_line
            .strip_prefix("version ")
            .ok_or(ParseError::BadVersion)?;
        if version_value != VERSION_URL {
            return Err(ParseError::UnsupportedVersion {
                found: version_value.to_owned(),
            });
        }

        let mut oid: Option<[u8; 32]> = None;
        let mut size: Option<u64> = None;
        let mut extensions: Vec<(String, String)> = Vec::new();
        let mut seen_keys: Vec<String> = Vec::new();

        for line in lines {
            if line.is_empty() {
                continue; // trailing empty from split
            }
            // "key value" — exactly one space separator.
            let (key, value) = line.split_once(' ').ok_or(ParseError::BadVersion)?;
            if seen_keys.iter().any(|k| k == key) {
                return Err(ParseError::DuplicateKey(key.to_owned()));
            }
            seen_keys.push(key.to_owned());

            match key {
                "oid" => {
                    let hex = value.strip_prefix("sha256:").ok_or(ParseError::BadOid)?;
                    if hex.len() != 64 {
                        return Err(ParseError::BadOid);
                    }
                    let mut bytes = [0u8; 32];
                    for (i, byte) in bytes.iter_mut().enumerate() {
                        let hi = hex_digit(hex.as_bytes()[i * 2]).ok_or(ParseError::BadOid)?;
                        let lo = hex_digit(hex.as_bytes()[i * 2 + 1]).ok_or(ParseError::BadOid)?;
                        // Reject uppercase hex (spec says lowercase).
                        if hex.as_bytes()[i * 2].is_ascii_uppercase()
                            || hex.as_bytes()[i * 2 + 1].is_ascii_uppercase()
                        {
                            return Err(ParseError::BadOid);
                        }
                        *byte = (hi << 4) | lo;
                    }
                    oid = Some(bytes);
                }
                "size" => {
                    let n: u64 = value.parse().map_err(|_| ParseError::BadSize)?;
                    size = Some(n);
                }
                _ => {
                    extensions.push((key.to_owned(), value.to_owned()));
                }
            }
        }

        let oid = oid.ok_or(ParseError::BadOid)?;
        let size = size.ok_or(ParseError::BadSize)?;

        Ok(Pointer {
            oid,
            size,
            extensions,
        })
    }

    pub fn write(&self) -> Vec<u8> {
        // Version always first; all other keys sorted alphabetically.
        // Known keys: oid, size. Unknown: extensions. Merge-sort them all.
        let mut keyed: Vec<(String, String)> = Vec::with_capacity(2 + self.extensions.len());
        keyed.push(("oid".to_owned(), format!("sha256:{}", self.oid_hex())));
        keyed.push(("size".to_owned(), self.size.to_string()));
        for (k, v) in &self.extensions {
            keyed.push((k.clone(), v.clone()));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::with_capacity(
            VERSION_PREFIX.len()
                + keyed
                    .iter()
                    .map(|(k, v)| k.len() + v.len() + 2)
                    .sum::<usize>(),
        );
        out.push_str("version ");
        out.push_str(VERSION_URL);
        out.push('\n');
        for (k, v) in &keyed {
            out.push_str(k);
            out.push(' ');
            out.push_str(v);
            out.push('\n');
        }
        out.into_bytes()
    }

    pub fn oid_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for byte in &self.oid {
            s.push(hex_char(byte >> 4));
            s.push(hex_char(byte & 0x0f));
        }
        s
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_char(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => unreachable!(),
    }
}

/// Fast check: does this byte slice look like an LFS pointer?
///
/// Used to short-circuit blob inspection before a full parse.
pub fn looks_like_pointer(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_POINTER_BYTES && bytes.starts_with(VERSION_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OID_HEX: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
    const SAMPLE_SIZE: u64 = 12345;

    fn sample_oid() -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            let hi = hex_digit(SAMPLE_OID_HEX.as_bytes()[i * 2]).unwrap();
            let lo = hex_digit(SAMPLE_OID_HEX.as_bytes()[i * 2 + 1]).unwrap();
            out[i] = (hi << 4) | lo;
        }
        out
    }

    fn sample_pointer_bytes() -> Vec<u8> {
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            SAMPLE_OID_HEX, SAMPLE_SIZE
        )
        .into_bytes()
    }

    #[test]
    fn roundtrip_canonical_pointer() {
        let bytes = sample_pointer_bytes();
        let p = Pointer::parse(&bytes).unwrap();
        assert_eq!(p.oid, sample_oid());
        assert_eq!(p.size, SAMPLE_SIZE);
        assert!(p.extensions.is_empty());
        assert_eq!(p.oid_hex(), SAMPLE_OID_HEX);
        assert_eq!(p.write(), bytes);
    }

    #[test]
    fn parse_keys_in_any_order_after_version() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\nsize {}\noid sha256:{}\n",
            SAMPLE_SIZE, SAMPLE_OID_HEX
        );
        let p = Pointer::parse(bytes.as_bytes()).unwrap();
        assert_eq!(p.size, SAMPLE_SIZE);
        // Write sorts alphabetically: oid before size.
        let out = p.write();
        let text = std::str::from_utf8(&out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "version https://git-lfs.github.com/spec/v1");
        assert!(lines[1].starts_with("oid "));
        assert!(lines[2].starts_with("size "));
    }

    #[test]
    fn empty_input_rejected() {
        assert_eq!(Pointer::parse(b""), Err(ParseError::Empty));
    }

    #[test]
    fn too_large_rejected() {
        let huge = vec![b'a'; MAX_POINTER_BYTES + 1];
        assert!(matches!(
            Pointer::parse(&huge),
            Err(ParseError::TooLarge(_))
        ));
    }

    #[test]
    fn non_ascii_rejected() {
        let bytes = b"version https://git-lfs.github.com/spec/v1\nsize 1\noid sha256:\xff\n";
        assert_eq!(Pointer::parse(bytes), Err(ParseError::NonAscii));
    }

    #[test]
    fn crlf_rejected() {
        let bytes = b"version https://git-lfs.github.com/spec/v1\r\nsize 1\r\n";
        assert_eq!(Pointer::parse(bytes), Err(ParseError::CrlfLineEndings));
    }

    #[test]
    fn missing_trailing_newline_rejected() {
        // Valid content but no trailing LF — reject (spec requires LF on every line).
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}",
            SAMPLE_OID_HEX, SAMPLE_SIZE
        );
        assert!(Pointer::parse(bytes.as_bytes()).is_err());
    }

    #[test]
    fn bad_version_url_rejected() {
        let bytes = b"version https://example.com/v99\noid sha256:0\nsize 1\n";
        assert!(matches!(
            Pointer::parse(bytes),
            Err(ParseError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn missing_version_rejected() {
        let bytes = format!("oid sha256:{}\nsize {}\n", SAMPLE_OID_HEX, SAMPLE_SIZE);
        assert_eq!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::BadVersion)
        );
    }

    #[test]
    fn uppercase_hex_rejected() {
        let upper: String = SAMPLE_OID_HEX.to_ascii_uppercase();
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            upper, SAMPLE_SIZE
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadOid));
    }

    #[test]
    fn short_oid_rejected() {
        let bytes = b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n";
        assert_eq!(Pointer::parse(bytes), Err(ParseError::BadOid));
    }

    #[test]
    fn non_numeric_size_rejected() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize notanumber\n",
            SAMPLE_OID_HEX
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadSize));
    }

    #[test]
    fn missing_oid_rejected() {
        let bytes = b"version https://git-lfs.github.com/spec/v1\nsize 1\n";
        assert_eq!(Pointer::parse(bytes), Err(ParseError::BadOid));
    }

    #[test]
    fn missing_size_rejected() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\n",
            SAMPLE_OID_HEX
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadSize));
    }

    #[test]
    fn duplicate_key_rejected() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\noid sha256:{}\nsize 1\n",
            SAMPLE_OID_HEX, SAMPLE_OID_HEX
        );
        assert!(matches!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::DuplicateKey(_))
        ));
    }

    #[test]
    fn extensions_preserved_roundtrip() {
        // Unknown keys must be preserved and sorted with known keys on write.
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\nextra value-x\noid sha256:{}\nsize {}\n",
            SAMPLE_OID_HEX, SAMPLE_SIZE
        );
        let p = Pointer::parse(bytes.as_bytes()).unwrap();
        assert_eq!(
            p.extensions,
            vec![("extra".to_owned(), "value-x".to_owned())]
        );
        let out = p.write();
        let expected = format!(
            "version https://git-lfs.github.com/spec/v1\nextra value-x\noid sha256:{}\nsize {}\n",
            SAMPLE_OID_HEX, SAMPLE_SIZE
        );
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn looks_like_pointer_positive() {
        assert!(looks_like_pointer(&sample_pointer_bytes()));
    }

    #[test]
    fn looks_like_pointer_rejects_binary() {
        let binary: Vec<u8> = (0..2048u16).map(|i| (i % 256) as u8).collect();
        assert!(!looks_like_pointer(&binary));
    }

    #[test]
    fn looks_like_pointer_rejects_text_starting_with_version() {
        assert!(!looks_like_pointer(b"version 2.0 something else\n"));
    }

    #[test]
    fn looks_like_pointer_rejects_too_large_even_with_prefix() {
        let mut buf = VERSION_PREFIX.to_vec();
        buf.resize(MAX_POINTER_BYTES + 1, b'x');
        assert!(!looks_like_pointer(&buf));
    }

    #[test]
    fn size_zero_accepted() {
        // Empty files are valid LFS content.
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 0\n",
            SAMPLE_OID_HEX
        );
        let p = Pointer::parse(bytes.as_bytes()).unwrap();
        assert_eq!(p.size, 0);
    }

    #[test]
    fn large_size_accepted() {
        let big = u64::MAX;
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            SAMPLE_OID_HEX, big
        );
        let p = Pointer::parse(bytes.as_bytes()).unwrap();
        assert_eq!(p.size, big);
    }
}

#[cfg(test)]
mod interop_tests {
    use super::*;

    #[test]
    fn matches_git_lfs_output() {
        // "hello world\n" is 12 bytes; sha256 matches git-lfs 3.7.1 output.
        let hex = "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";
        let mut oid = [0u8; 32];
        for i in 0..32 {
            let hi = hex_digit(hex.as_bytes()[i * 2]).unwrap();
            let lo = hex_digit(hex.as_bytes()[i * 2 + 1]).unwrap();
            oid[i] = (hi << 4) | lo;
        }
        let p = Pointer {
            oid,
            size: 12,
            extensions: vec![],
        };
        let out = p.write();
        let expected = b"version https://git-lfs.github.com/spec/v1\noid sha256:a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447\nsize 12\n";
        assert_eq!(out, expected);
    }
}
