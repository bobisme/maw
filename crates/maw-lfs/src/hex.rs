//! Shared hex codec for LFS object ids (sha256, 32 bytes / 64 hex chars).
//!
//! One implementation for every maw-lfs call site (pointer codec, object
//! store paths, batch API), so the nibble tables cannot drift apart. The
//! Kani harnesses in [`crate::kani_proofs`] check the nibble codec over its
//! entire domain.

/// Which hex letter cases a decoder accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexCase {
    /// Only `0-9a-f`. The LFS pointer spec requires lowercase oids, and
    /// git-lfs rejects uppercase pointer oids.
    LowerOnly,
    /// `0-9a-f` and `A-F`. Used for values that come from remote servers,
    /// where rejecting uppercase would gain nothing.
    AnyCase,
}

/// Value of one hex digit, or `None` for a byte that is not a hex digit in
/// the accepted `case`.
#[must_use]
pub const fn nibble_value(b: u8, case: HexCase) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' if matches!(case, HexCase::AnyCase) => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Lowercase hex digit for a nibble. Only the low four bits of `n` are used.
#[must_use]
pub const fn nibble_char(n: u8) -> u8 {
    let n = n & 0x0f;
    if n < 10 { b'0' + n } else { b'a' + n - 10 }
}

/// Encode a 32-byte oid as 64 lowercase hex characters.
#[must_use]
pub fn encode_oid(oid: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for &byte in oid {
        s.push(char::from(nibble_char(byte >> 4)));
        s.push(char::from(nibble_char(byte & 0x0f)));
    }
    s
}

/// Decode exactly 64 hex characters into a 32-byte oid.
///
/// Works on bytes, so non-ASCII input is rejected instead of panicking on a
/// char boundary. Signs (`+`), whitespace, and any length other than 64 are
/// rejected.
#[must_use]
pub fn decode_oid(hex: &[u8], case: HexCase) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = nibble_value(hex[i * 2], case)?;
        let lo = nibble_value(hex[i * 2 + 1], case)?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_round_trip_all_byte_values() {
        let mut oid = [0u8; 32];
        for start in (0u16..256).step_by(32) {
            for (i, b) in oid.iter_mut().enumerate() {
                *b = u8::try_from(start + u16::try_from(i).expect("small")).expect("byte");
            }
            let hex = encode_oid(&oid);
            assert_eq!(decode_oid(hex.as_bytes(), HexCase::LowerOnly), Some(oid));
        }
    }

    #[test]
    fn decode_rejects_uppercase_only_in_lower_mode() {
        let hex = "A".repeat(64);
        assert_eq!(decode_oid(hex.as_bytes(), HexCase::LowerOnly), None);
        assert_eq!(
            decode_oid(hex.as_bytes(), HexCase::AnyCase),
            Some([0xaa; 32])
        );
    }

    #[test]
    fn decode_rejects_sign_and_non_ascii_without_panicking() {
        let mut hex = "0".repeat(62);
        hex.push_str("+f");
        assert_eq!(decode_oid(hex.as_bytes(), HexCase::AnyCase), None);
        // 64 bytes, but multi-byte UTF-8 straddles an even boundary.
        let mut hex = "0".repeat(61);
        hex.push('\u{e9}'); // 2 bytes
        hex.push('0');
        assert_eq!(hex.len(), 64);
        assert_eq!(decode_oid(hex.as_bytes(), HexCase::AnyCase), None);
    }
}
