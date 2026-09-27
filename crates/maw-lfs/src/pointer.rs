//! LFS pointer format v1 codec.
//!
//! Spec: <https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md>
//!
//! Canonical form (the only form [`Pointer::parse`] accepts and the only
//! form [`Pointer::write`] produces):
//!
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! ext-<digit>-<name> sha256:<64-char-lowercase-hex>     (0..=10 lines)
//! oid sha256:<64-char-lowercase-hex>
//! size <decimal-bytes>
//! ```
//!
//! # Acceptance policy
//!
//! A false positive (non-pointer content classified as a pointer) makes maw
//! smudge or skip a file that git-lfs would treat as plain content. So the
//! accept set is chosen to be:
//!
//! - a SUBSET of what `git lfs pointer --check` accepts (git-lfs 3.8), so
//!   maw never classifies as a pointer something git-lfs would not; and
//! - a SUPERSET of what git-lfs writes (`git lfs clean` / `git lfs pointer`),
//!   so every real pointer is still recognised.
//!
//! Concretely:
//! - `version` line first, with the exact `git-lfs.github.com/spec/v1` URL.
//!   (git-lfs also accepts two legacy aliases; maw never has.)
//! - Then extension lines, then `oid`, then `size`, then end of input. This
//!   is the order git-lfs writes and the order the spec requires (`version`
//!   first, the rest sorted). git-lfs rejects `size` before `oid` and any
//!   line after `size`.
//! - The only unknown keys git-lfs accepts are extensions: `ext-<digit>-<name>`
//!   with a `sha256:<hex>` value and distinct priorities. Anything else
//!   (for example `extra value`) is rejected, as git-lfs rejects it.
//! - Extensions must be in strictly ascending priority (= key) order.
//! - `size` is canonical decimal: digits only, no sign, no leading zero,
//!   at most `i64::MAX` (git-lfs parses it as a signed 64-bit integer).
//! - `oid` is exactly 64 lowercase hex digits.
//! - Every line ends with LF, there are no empty lines, no CR, ASCII only.
//! - At most 1024 bytes.
//!
//! These rules make the codec a bijection on its accept set:
//! `parse(b) == Ok(p)` implies `p.write() == Ok(b)`, and for every `p` that
//! [`Pointer::validate`] accepts, `parse(p.write()) == Ok(p)`.

use thiserror::Error;

use crate::hex::{HexCase, decode_oid, encode_oid};

const VERSION_URL: &str = "https://git-lfs.github.com/spec/v1";
/// Largest pointer blob, in bytes. git-lfs reads at most this many bytes
/// when it decides whether a blob is a pointer.
pub const MAX_POINTER_BYTES: usize = 1024;
const VERSION_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";
const OID_VALUE_PREFIX: &str = "sha256:";
/// git-lfs stores the size in an `int64`.
const MAX_SIZE: u64 = i64::MAX as u64;

/// A parsed LFS pointer. Represents the content of a git blob that stands in
/// for a real binary file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// sha256 of the real file content.
    pub oid: [u8; 32],
    /// Size of the real file, in bytes. Must be at most `i64::MAX`.
    pub size: u64,
    /// Pointer extension lines, as `(key, value)` in file order, for
    /// example `("ext-0-foo", "sha256:<hex>")`. Preserved on write.
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
    /// `found` is the raw version value (ASCII; kept as bytes so the
    /// envelope check stays cheap for Kani).
    #[error("unsupported pointer version: {}", String::from_utf8_lossy(found))]
    UnsupportedVersion { found: Vec<u8> },
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
    #[error("line is not of the form '<key> <value>'")]
    MalformedLine,
    #[error("empty line in pointer")]
    EmptyLine,
    #[error("unexpected key (not allowed here): {0}")]
    UnexpectedKey(String),
    #[error("invalid or out-of-order pointer extension: {0}")]
    BadExtension(String),
}

impl Pointer {
    /// Parse an LFS pointer from canonical pointer bytes.
    ///
    /// See the module docs for the exact grammar.
    ///
    /// # Errors
    /// Returns a [`ParseError`] if the bytes are not a canonical pointer.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let rest = check_envelope(bytes)?;

        let mut oid: Option<[u8; 32]> = None;
        let mut size: Option<u64> = None;
        let mut extensions: Vec<(String, String)> = Vec::new();

        // `rest` is empty when there are no lines after the version line.
        let lines = (!rest.is_empty()).then(|| rest.split(|&b| b == b'\n'));
        for line in lines.into_iter().flatten() {
            if line.is_empty() {
                return Err(ParseError::EmptyLine);
            }
            // "key value" — split at the first space.
            let split = line
                .iter()
                .position(|&b| b == b' ')
                .ok_or(ParseError::MalformedLine)?;
            let (key, value) = (&line[..split], &line[split + 1..]);

            let duplicate = match key {
                b"version" => true,
                b"oid" => oid.is_some(),
                b"size" => size.is_some(),
                _ => extensions.iter().any(|(k, _)| k.as_bytes() == key),
            };
            if duplicate {
                return Err(ParseError::DuplicateKey(ascii_string(key)));
            }

            match key {
                b"oid" => {
                    oid = Some(parse_oid_value(value).ok_or(ParseError::BadOid)?);
                }
                b"size" => {
                    if oid.is_none() {
                        // `size` before `oid` (or no `oid` at all).
                        return Err(ParseError::BadOid);
                    }
                    size = Some(parse_size(value).ok_or(ParseError::BadSize)?);
                }
                _ => {
                    // Extensions must come before `oid`. This also rejects
                    // any line after `size` (which requires `oid`): `oid`,
                    // `size` and `version` there are duplicates.
                    if oid.is_some() {
                        return Err(ParseError::UnexpectedKey(ascii_string(key)));
                    }
                    let key = ascii_string(key);
                    let value = ascii_string(value);
                    check_extension(extensions.last().map(|(k, _)| k.as_str()), &key, &value)?;
                    extensions.push((key, value));
                }
            }
        }

        let oid = oid.ok_or(ParseError::BadOid)?;
        let size = size.ok_or(ParseError::BadSize)?;

        Ok(Self {
            oid,
            size,
            extensions,
        })
    }

    /// Check that this pointer can be written in canonical form, that is,
    /// that [`Pointer::write`] will succeed and [`Pointer::parse`] will read
    /// the result back as an equal pointer.
    ///
    /// # Errors
    /// Returns [`ParseError::BadSize`] for a size above `i64::MAX`,
    /// [`ParseError::BadExtension`] / [`ParseError::DuplicateKey`] for an
    /// invalid or out-of-order extension, and [`ParseError::TooLarge`] if
    /// the encoding would exceed [`MAX_POINTER_BYTES`].
    pub fn validate(&self) -> Result<(), ParseError> {
        if self.size > MAX_SIZE {
            return Err(ParseError::BadSize);
        }
        let mut prev: Option<&str> = None;
        for (key, value) in &self.extensions {
            check_extension(prev, key, value)?;
            prev = Some(key);
        }
        let len = self.encoded_len();
        if len > MAX_POINTER_BYTES {
            return Err(ParseError::TooLarge(len));
        }
        Ok(())
    }

    /// Serialize this pointer in canonical Git LFS pointer format.
    ///
    /// # Errors
    /// Returns the [`Pointer::validate`] error if this pointer has no
    /// canonical encoding. A pointer with no extensions and a size of at
    /// most `i64::MAX` always encodes.
    pub fn write(&self) -> Result<Vec<u8>, ParseError> {
        self.validate()?;
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(VERSION_PREFIX);
        for (k, v) in &self.extensions {
            out.extend_from_slice(k.as_bytes());
            out.push(b' ');
            out.extend_from_slice(v.as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(b"oid ");
        out.extend_from_slice(OID_VALUE_PREFIX.as_bytes());
        out.extend_from_slice(encode_oid(&self.oid).as_bytes());
        out.extend_from_slice(b"\nsize ");
        push_decimal(&mut out, self.size);
        out.push(b'\n');
        Ok(out)
    }

    #[must_use]
    pub fn oid_hex(&self) -> String {
        encode_oid(&self.oid)
    }

    fn encoded_len(&self) -> usize {
        let ext: usize = self
            .extensions
            .iter()
            .map(|(k, v)| k.len() + v.len() + 2)
            .sum();
        // "oid sha256:<64>\n" + "size <digits>\n"
        VERSION_PREFIX.len() + ext + 4 + 7 + 64 + 1 + 5 + decimal_len(self.size) + 1
    }
}

const fn decimal_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}

/// Append the canonical decimal form of `n` (no sign, no leading zero).
/// Inverse of [`parse_size`] on `0..=i64::MAX`.
pub(crate) fn push_decimal(out: &mut Vec<u8>, mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        // n % 10 < 10, so the cast cannot truncate.
        #[allow(clippy::cast_possible_truncation)]
        let digit = (n % 10) as u8;
        buf[i] = b'0' + digit;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// The input is known to be ASCII, so this is lossless.
fn ascii_string(bytes: &[u8]) -> String {
    bytes.iter().copied().map(char::from).collect()
}

/// `sha256:<64 lowercase hex>`.
fn parse_oid_value(value: &[u8]) -> Option<[u8; 32]> {
    let hex = value.strip_prefix(OID_VALUE_PREFIX.as_bytes())?;
    decode_oid(hex, HexCase::LowerOnly)
}

/// Checks shared by every pointer: size, ASCII, LF-only line endings, and
/// the exact version line. On success returns the remaining lines (after the
/// version line), without the final LF; empty if there are none.
///
/// `Pointer::parse` calls this first, so `parse(b).is_ok()` implies
/// `check_envelope(b).is_ok()`, which (Kani-checked) implies
/// `looks_like_pointer(b)`.
pub(crate) fn check_envelope(bytes: &[u8]) -> Result<&[u8], ParseError> {
    if bytes.is_empty() {
        return Err(ParseError::Empty);
    }
    if bytes.len() > MAX_POINTER_BYTES {
        return Err(ParseError::TooLarge(bytes.len()));
    }
    // Plain byte loops rather than `is_ascii`/`contains`: same result, and
    // tractable for Kani (the word-at-a-time std versions are not).
    let mut has_cr = false;
    for &b in bytes {
        if b >= 0x80 {
            return Err(ParseError::NonAscii);
        }
        has_cr |= b == b'\r';
    }
    if has_cr {
        return Err(ParseError::CrlfLineEndings);
    }
    // Every line, including the last, must terminate in LF.
    let Some(body) = bytes.strip_suffix(b"\n") else {
        return Err(ParseError::BadVersion);
    };
    let (version_line, rest): (&[u8], &[u8]) = body
        .iter()
        .position(|&b| b == b'\n')
        .map_or((body, &[]), |i| (&body[..i], &body[i + 1..]));
    // Version line is exactly "version <URL>".
    let version_value = version_line
        .strip_prefix(b"version ")
        .ok_or(ParseError::BadVersion)?;
    if version_value != VERSION_URL.as_bytes() {
        return Err(ParseError::UnsupportedVersion {
            found: version_value.to_vec(),
        });
    }
    Ok(rest)
}

/// Canonical decimal: no sign, no leading zero (except `0`), at most
/// `i64::MAX`.
pub(crate) fn parse_size(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if value.len() > 1 && value[0] == b'0' {
        return None;
    }
    let mut n: u64 = 0;
    for &d in value {
        n = n.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
    }
    (n <= MAX_SIZE).then_some(n)
}

/// Priority digit of an extension key `ext-<digit>-<name>`, where `<name>`
/// is one or more of `[A-Za-z0-9_.-]` starting with `[A-Za-z0-9_]`.
///
/// git-lfs matches keys against `\Aext-\d{1}-\w+`; the name charset here is
/// that, plus the `.` and `-` the spec allows in keys.
fn extension_priority(key: &[u8]) -> Option<u8> {
    let rest = key.strip_prefix(b"ext-")?;
    let (&digit, rest) = rest.split_first()?;
    if !digit.is_ascii_digit() {
        return None;
    }
    let name = rest.strip_prefix(b"-")?;
    let (&first, _) = name.split_first()?;
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    if !word(first) || !name.iter().all(|&b| word(b) || b == b'.' || b == b'-') {
        return None;
    }
    Some(digit - b'0')
}

/// Validate one extension line given the previous extension key.
fn check_extension(prev: Option<&str>, key: &str, value: &str) -> Result<(), ParseError> {
    let bad = || ParseError::BadExtension(key.to_owned());
    let Some(priority) = extension_priority(key.as_bytes()) else {
        return Err(ParseError::UnexpectedKey(key.to_owned()));
    };
    if parse_oid_value(value.as_bytes()).is_none() {
        return Err(bad());
    }
    if let Some(prev) = prev {
        if prev == key {
            return Err(ParseError::DuplicateKey(key.to_owned()));
        }
        // git-lfs rejects duplicate priorities; ascending order is the order
        // git-lfs writes and the sorted order the spec requires.
        let prev_priority = extension_priority(prev.as_bytes()).ok_or_else(bad)?;
        if priority <= prev_priority {
            return Err(bad());
        }
    }
    Ok(())
}

/// Would git-lfs's clean filter hand `bytes` to git unchanged (because it
/// already decodes as a pointer) instead of storing it as a new object?
///
/// Mirrors git-lfs 3.x `copyToTemp` + `DecodeFrom` + `decodeKV`
/// (`lfs/gitfilter_clean.go`, `lfs/pointer.go`), which is deliberately more
/// lenient than [`Pointer::parse`]: surrounding whitespace, blank lines and a
/// trailing `\r` per line are ignored; the legacy `hawser` / `git-media`
/// version URLs are accepted; `size` is any non-negative `i64` Go's
/// `ParseInt` accepts (`012`, `+12`); `ext-<d>-<name>` lines may appear
/// anywhere before `size`. Content must be shorter than
/// [`MAX_POINTER_BYTES`]. Empty content is git-lfs's empty pointer.
///
/// Callers that decide "already a pointer, write as-is" must use this, not
/// the canonical parser: wrapping a pointer git-lfs keeps would commit a
/// pointer to a pointer (bn-ggo5).
#[must_use]
pub fn git_lfs_clean_passes_through(bytes: &[u8]) -> bool {
    const KEYS: [&[u8]; 3] = [b"version", b"oid", b"size"];
    if bytes.is_empty() {
        return true;
    }
    if bytes.len() >= MAX_POINTER_BYTES {
        return false;
    }
    let data = go_trim_ascii_space(bytes);
    let contains = |needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
    if !(contains(b"git-media") || contains(b"hawser") || contains(b"git-lfs")) {
        return false;
    }
    let mut values: [Option<&[u8]>; 3] = [None; 3];
    let mut exts: Vec<(&[u8], &[u8])> = Vec::new();
    let mut line = 0;
    for raw in data.split(|&b| b == b'\n') {
        let text = raw.strip_suffix(b"\r").unwrap_or(raw);
        if text.is_empty() {
            continue;
        }
        let Some(sp) = text.iter().position(|&b| b == b' ') else {
            return false;
        };
        let (key, value) = (&text[..sp], &text[sp + 1..]);
        if line >= KEYS.len() {
            return false;
        }
        if key == KEYS[line] {
            values[line] = Some(value);
            line += 1;
            continue;
        }
        // extRE `\Aext-\d{1}-\w+` (prefix match); git-lfs keeps extensions
        // in a map, so a repeated key overwrites.
        let ext_key = matches!(
            key,
            [b'e', b'x', b't', b'-', d, b'-', w, ..]
                if d.is_ascii_digit() && (w.is_ascii_alphanumeric() || *w == b'_')
        );
        if !ext_key {
            return false;
        }
        match exts.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => exts.push((key, value)),
        }
    }
    // parsePointerExtension + validatePointerExtensions.
    let mut priorities = [false; 10];
    for (key, value) in &exts {
        let p = usize::from(key[4] - b'0');
        if git_lfs_oid(value).is_none() || priorities[p] {
            return false;
        }
        priorities[p] = true;
    }
    let version_ok = values[0].is_some_and(|v| {
        [
            b"http://git-media.io/v/2".as_slice(),
            b"https://hawser.github.com/spec/v1",
            VERSION_URL.as_bytes(),
        ]
        .contains(&v)
    });
    version_ok
        && values[1].and_then(git_lfs_oid).is_some()
        && values[2].is_some_and(go_parse_nonneg_i64)
}

/// Go `bytes.TrimSpace` restricted to ASCII (adds `\v`, which Rust's
/// `trim_ascii` keeps).
fn go_trim_ascii_space(mut b: &[u8]) -> &[u8] {
    let space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    while let Some((first, rest)) = b.split_first()
        && space(first)
    {
        b = rest;
    }
    while let Some((last, rest)) = b.split_last()
        && space(last)
    {
        b = rest;
    }
    b
}

/// git-lfs `parseOid`: `sha256:` followed by exactly 64 lowercase hex digits.
fn git_lfs_oid(value: &[u8]) -> Option<[u8; 32]> {
    let (kind, hex) = value.split_at(value.iter().position(|&b| b == b':')?);
    if kind != b"sha256" {
        return None;
    }
    decode_oid(&hex[1..], HexCase::LowerOnly)
}

/// Go `strconv.ParseInt(s, 10, 64)` succeeding with a non-negative result.
fn go_parse_nonneg_i64(value: &[u8]) -> bool {
    let (negative, digits) = match value.split_first() {
        Some((b'+', rest)) => (false, rest),
        Some((b'-', rest)) => (true, rest),
        _ => (false, value),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let mut n: u64 = 0;
    for &d in digits {
        match n
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(d - b'0')))
        {
            Some(v) => n = v,
            None => return false,
        }
    }
    if negative { n == 0 } else { n <= MAX_SIZE }
}

/// Fast check: does this byte slice look like an LFS pointer?
///
/// Used to short-circuit blob inspection before a full parse. It is a
/// necessary condition for [`Pointer::parse`] to succeed, not a sufficient
/// one: `Pointer::parse(b).is_ok()` implies `looks_like_pointer(b)`.
#[must_use]
pub fn looks_like_pointer(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_POINTER_BYTES && bytes.starts_with(VERSION_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OID_HEX: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
    const SAMPLE_SIZE: u64 = 12345;

    fn sample_oid() -> [u8; 32] {
        decode_oid(SAMPLE_OID_HEX.as_bytes(), HexCase::LowerOnly).expect("valid hex")
    }

    fn sample_pointer_bytes() -> Vec<u8> {
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize {SAMPLE_SIZE}\n"
        )
        .into_bytes()
    }

    #[test]
    fn roundtrip_canonical_pointer() {
        let bytes = sample_pointer_bytes();
        let p = Pointer::parse(&bytes).expect("operation should succeed");
        assert_eq!(p.oid, sample_oid());
        assert_eq!(p.size, SAMPLE_SIZE);
        assert!(p.extensions.is_empty());
        assert_eq!(p.oid_hex(), SAMPLE_OID_HEX);
        assert_eq!(p.write().expect("valid pointer"), bytes);
    }

    #[test]
    fn size_before_oid_rejected() {
        // git-lfs rejects this ("expected key oid, got size"); so does maw.
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\nsize {SAMPLE_SIZE}\noid sha256:{SAMPLE_OID_HEX}\n"
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadOid));
    }

    /// bn-ggo5: pass-through decision table, observed against real
    /// git-lfs 3.8 `git lfs clean` (the conformance test re-checks it live
    /// when git-lfs is installed).
    #[test]
    fn git_lfs_clean_passes_through_matches_observed_git_lfs() {
        let o = SAMPLE_OID_HEX;
        let v = "version https://git-lfs.github.com/spec/v1";
        let cases: &[(&str, String, bool)] = &[
            ("empty", String::new(), true),
            ("canonical", format!("{v}\noid sha256:{o}\nsize 12\n"), true),
            (
                "no final newline",
                format!("{v}\noid sha256:{o}\nsize 12"),
                true,
            ),
            (
                "interior blank",
                format!("{v}\n\noid sha256:{o}\nsize 12\n"),
                true,
            ),
            (
                "crlf",
                format!("{v}\r\noid sha256:{o}\r\nsize 12\r\n"),
                true,
            ),
            (
                "hawser",
                format!("version https://hawser.github.com/spec/v1\noid sha256:{o}\nsize 12\n"),
                true,
            ),
            (
                "leading zero",
                format!("{v}\noid sha256:{o}\nsize 012\n"),
                true,
            ),
            (
                "ext after oid",
                format!("{v}\noid sha256:{o}\next-0-x sha256:{o}\nsize 12\n"),
                true,
            ),
            (
                "upper hex",
                format!("{v}\noid sha256:{}\nsize 12\n", o.to_uppercase()),
                false,
            ),
            (
                "size first",
                format!("{v}\nsize 12\noid sha256:{o}\n"),
                false,
            ),
            (
                "extra key",
                format!("{v}\noid sha256:{o}\nsize 12\nfoo bar\n"),
                false,
            ),
            ("negative", format!("{v}\noid sha256:{o}\nsize -1\n"), false),
            ("prose", format!("{v}\nThis file documents LFS.\n"), false),
            (
                "dup priority",
                format!("{v}\next-0-a sha256:{o}\next-0-b sha256:{o}\noid sha256:{o}\nsize 1\n"),
                false,
            ),
            (
                "too big",
                format!("{v}\noid sha256:{o}\nsize 12\n{}", "\n".repeat(1100)),
                false,
            ),
        ];
        for (name, data, expected) in cases {
            assert_eq!(
                git_lfs_clean_passes_through(data.as_bytes()),
                *expected,
                "{name}: {data:?}"
            );
        }
    }

    #[test]
    fn non_canonical_size_rejected() {
        // git-lfs accepts some of these under --check but never writes them
        // and --strict rejects them; maw rejects so parse/write stay a
        // bijection.
        for size in ["+5", "05", "-0", "-5", "5 ", " 5", "5_0", ""] {
            let bytes = format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize {size}\n"
            );
            assert_eq!(
                Pointer::parse(bytes.as_bytes()),
                Err(ParseError::BadSize),
                "size {size:?}"
            );
        }
    }

    #[test]
    fn line_after_size_rejected() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize 1\nfoo bar\n"
        );
        assert_eq!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::UnexpectedKey("foo".to_owned()))
        );
    }

    #[test]
    fn empty_line_rejected() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\n\noid sha256:{SAMPLE_OID_HEX}\nsize 1\n"
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::EmptyLine));
    }

    #[test]
    fn non_extension_unknown_key_rejected() {
        // git-lfs only accepts unknown keys matching ext-<digit>-<name>.
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\nextra value-x\noid sha256:{SAMPLE_OID_HEX}\nsize 1\n"
        );
        assert_eq!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::UnexpectedKey("extra".to_owned()))
        );
    }

    #[test]
    fn extension_rules() {
        let o = SAMPLE_OID_HEX;
        let v = "version https://git-lfs.github.com/spec/v1";
        let tail = format!("oid sha256:{o}\nsize 1\n");
        let ok = [
            format!("{v}\next-0-foo sha256:{o}\n{tail}"),
            format!("{v}\next-0-Foo_x sha256:{o}\next-3-a.b-c sha256:{o}\n{tail}"),
        ];
        for bytes in &ok {
            let p = Pointer::parse(bytes.as_bytes()).expect("valid extension pointer");
            assert_eq!(p.write().expect("valid"), bytes.as_bytes());
        }
        let bad = [
            // value is not an oid
            format!("{v}\next-0-foo bar\n{tail}"),
            // duplicate priority
            format!("{v}\next-0-foo sha256:{o}\next-0-goo sha256:{o}\n{tail}"),
            // descending priority
            format!("{v}\next-1-foo sha256:{o}\next-0-goo sha256:{o}\n{tail}"),
            // after oid
            format!("{v}\noid sha256:{o}\next-0-foo sha256:{o}\nsize 1\n"),
            // two-digit priority / missing name / bad first name char
            format!("{v}\next-10-foo sha256:{o}\n{tail}"),
            format!("{v}\next-1- sha256:{o}\n{tail}"),
            format!("{v}\next-1-.x sha256:{o}\n{tail}"),
        ];
        for bytes in &bad {
            assert!(Pointer::parse(bytes.as_bytes()).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn write_refuses_malformed_pointers() {
        let base = Pointer {
            oid: sample_oid(),
            size: 1,
            extensions: vec![],
        };
        let ext = |k: &str, v: &str| Pointer {
            extensions: vec![(k.to_owned(), v.to_owned())],
            ..base.clone()
        };
        let good_val = format!("sha256:{SAMPLE_OID_HEX}");
        for p in [
            Pointer {
                size: u64::MAX,
                ..base.clone()
            },
            ext("oid", &good_val),
            ext("size", &good_val),
            ext("version", &good_val),
            ext("ext-0-a b", &good_val),
            ext("ext-0-a", "x\ny"),
            ext("ext-0-a", "sha256:zz"),
            Pointer {
                extensions: vec![
                    ("ext-1-a".to_owned(), good_val.clone()),
                    ("ext-0-a".to_owned(), good_val.clone()),
                ],
                ..base.clone()
            },
            Pointer {
                extensions: (0..10)
                    .map(|i| (format!("ext-{i}-{}", "n".repeat(60)), good_val.clone()))
                    .collect(),
                ..base.clone()
            },
        ] {
            assert!(p.write().is_err(), "{p:?}");
        }
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
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize {SAMPLE_SIZE}"
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
        let bytes = format!("oid sha256:{SAMPLE_OID_HEX}\nsize {SAMPLE_SIZE}\n");
        assert_eq!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::BadVersion)
        );
    }

    #[test]
    fn uppercase_hex_rejected() {
        let upper: String = SAMPLE_OID_HEX.to_ascii_uppercase();
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{upper}\nsize {SAMPLE_SIZE}\n"
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
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize notanumber\n"
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
        let bytes =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\n");
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadSize));
    }

    #[test]
    fn duplicate_key_rejected() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\noid sha256:{SAMPLE_OID_HEX}\nsize 1\n"
        );
        assert!(matches!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::DuplicateKey(_))
        ));
    }

    #[test]
    fn extensions_preserved_roundtrip() {
        // Extension lines must be preserved and written before oid.
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\next-0-foo sha256:{SAMPLE_OID_HEX}\noid sha256:{SAMPLE_OID_HEX}\nsize {SAMPLE_SIZE}\n"
        );
        let p = Pointer::parse(bytes.as_bytes()).expect("operation should succeed");
        assert_eq!(
            p.extensions,
            vec![("ext-0-foo".to_owned(), format!("sha256:{SAMPLE_OID_HEX}"))]
        );
        let out = p.write().expect("valid pointer");
        assert_eq!(out, bytes.as_bytes());
    }

    #[test]
    fn looks_like_pointer_positive() {
        assert!(looks_like_pointer(&sample_pointer_bytes()));
    }

    #[test]
    fn looks_like_pointer_rejects_binary() {
        let binary: Vec<u8> = (0..2048u16)
            .map(|i| u8::try_from(i % 256).expect("value reduced below byte range"))
            .collect();
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
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize 0\n"
        );
        let p = Pointer::parse(bytes.as_bytes()).expect("operation should succeed");
        assert_eq!(p.size, 0);
    }

    #[test]
    fn size_above_i64_max_rejected() {
        // git-lfs stores size as int64 and rejects larger values.
        let big = u64::MAX;
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize {big}\n"
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadSize));
        let over = MAX_SIZE + 1;
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize {over}\n"
        );
        assert_eq!(Pointer::parse(bytes.as_bytes()), Err(ParseError::BadSize));
    }

    /// A pointer with one extension whose name has `name_len` bytes.
    fn pointer_with_ext_name(name_len: usize, size: u64) -> Pointer {
        Pointer {
            oid: sample_oid(),
            size,
            extensions: vec![(
                format!("ext-0-{}", "n".repeat(name_len)),
                format!("sha256:{SAMPLE_OID_HEX}"),
            )],
        }
    }

    /// bn-3bjx (mutation gap): the `TooLarge` guard in `validate` relies on
    /// `encoded_len` being exact; pin it to the real `write` output across
    /// extension counts and every decimal-width boundary of `size`.
    #[test]
    fn encoded_len_matches_written_length() {
        let ext = |k: &str| (k.to_owned(), format!("sha256:{SAMPLE_OID_HEX}"));
        let ext_sets = [
            vec![],
            vec![ext("ext-0-a")],
            vec![ext("ext-0-a"), ext("ext-1-bb")],
        ];
        let sizes = [0, 9, 10, 99, 100, 12_345, 1_000_000_000, MAX_SIZE];
        for extensions in &ext_sets {
            for &size in &sizes {
                let p = Pointer {
                    oid: sample_oid(),
                    size,
                    extensions: extensions.clone(),
                };
                let out = p.write().expect("valid pointer");
                assert_eq!(p.encoded_len(), out.len(), "size={size} ext={extensions:?}");
            }
        }
    }

    /// bn-3bjx (mutation gap): a pointer of exactly `MAX_POINTER_BYTES` is
    /// writable and parseable; one byte more is refused on both paths.
    #[test]
    fn max_pointer_bytes_boundary_is_inclusive() {
        // version(43) + ext line(6 + n + 1 + 71 + 1) + oid(76) + "size 0\n"(7)
        let n = MAX_POINTER_BYTES - (43 + 79 + 76 + 7);
        let at_max = pointer_with_ext_name(n, 0);
        let bytes = at_max
            .write()
            .expect("exactly MAX_POINTER_BYTES must encode");
        assert_eq!(bytes.len(), MAX_POINTER_BYTES);
        assert_eq!(Pointer::parse(&bytes), Ok(at_max));

        let over = pointer_with_ext_name(n + 1, 0);
        assert_eq!(
            over.validate(),
            Err(ParseError::TooLarge(MAX_POINTER_BYTES + 1))
        );
        let over_bytes = format!(
            "version https://git-lfs.github.com/spec/v1\next-0-{} sha256:{SAMPLE_OID_HEX}\noid sha256:{SAMPLE_OID_HEX}\nsize 0\n",
            "n".repeat(n + 1)
        );
        assert_eq!(over_bytes.len(), MAX_POINTER_BYTES + 1);
        assert_eq!(
            Pointer::parse(over_bytes.as_bytes()),
            Err(ParseError::TooLarge(MAX_POINTER_BYTES + 1))
        );
    }

    /// bn-3bjx (mutation gap): `size == i64::MAX` is the largest writable
    /// size; one more is refused.
    #[test]
    fn write_accepts_max_size_rejects_above() {
        let p = Pointer {
            oid: sample_oid(),
            size: MAX_SIZE,
            extensions: vec![],
        };
        let bytes = p.write().expect("i64::MAX must encode");
        assert_eq!(Pointer::parse(&bytes), Ok(p));
        let over = Pointer {
            oid: sample_oid(),
            size: MAX_SIZE + 1,
            extensions: vec![],
        };
        assert_eq!(over.write(), Err(ParseError::BadSize));
    }

    /// bn-3bjx (mutation gap): a repeated `version` line is reported as a
    /// duplicate key, not as an unknown key.
    #[test]
    fn repeated_version_line_is_duplicate_key() {
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\nversion https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize 1\n"
        );
        assert_eq!(
            Pointer::parse(bytes.as_bytes()),
            Err(ParseError::DuplicateKey("version".to_owned()))
        );
    }

    #[test]
    fn large_size_accepted() {
        let big = MAX_SIZE;
        let bytes = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{SAMPLE_OID_HEX}\nsize {big}\n"
        );
        let p = Pointer::parse(bytes.as_bytes()).expect("operation should succeed");
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
        let oid = decode_oid(hex.as_bytes(), HexCase::LowerOnly).expect("valid hex");
        let p = Pointer {
            oid,
            size: 12,
            extensions: vec![],
        };
        let out = p.write().expect("valid pointer");
        let expected = b"version https://git-lfs.github.com/spec/v1\noid sha256:a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447\nsize 12\n";
        assert_eq!(out, expected);
    }
}
