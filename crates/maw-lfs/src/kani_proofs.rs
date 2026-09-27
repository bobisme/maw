//! Kani harnesses for the LFS hex and pointer codecs (bn-z9t3).
//!
//! Run one harness: `cargo kani -p maw-lfs --harness <name>`.
//!
//! The hex harnesses are complete (every byte value). The pointer harnesses
//! are bounded and target the pieces of `Pointer::parse` / `Pointer::write`
//! that decide pointer-ness and canonicality: the envelope/version-line
//! check and the size field. Harness names state the bound. Running the whole
//! `parse` on even 44 symbolic bytes plus a concrete tail did not finish in
//! 7.5 minutes, so whole-pointer round trips are covered by
//! `tests/pointer_proptest.rs` instead.

use crate::hex::{HexCase, nibble_char, nibble_value};
use crate::pointer::{check_envelope, looks_like_pointer, parse_size, push_decimal};

fn any_case() -> HexCase {
    if kani::any() {
        HexCase::LowerOnly
    } else {
        HexCase::AnyCase
    }
}

/// Encode then decode is the identity on nibbles, for all 256 inputs
/// (`nibble_char` masks to the low four bits).
#[kani::proof]
fn hex_nibble_char_then_value_all_256_bytes() {
    let n: u8 = kani::any();
    let c = nibble_char(n);
    assert_eq!(nibble_value(c, HexCase::LowerOnly), Some(n & 0x0f));
    assert_eq!(nibble_value(c, HexCase::AnyCase), Some(n & 0x0f));
}

/// Decoding accepts exactly the hex digits of the chosen case, and every
/// accepted lowercase/digit byte re-encodes to itself.
#[kani::proof]
fn hex_nibble_value_exact_domain_all_256_bytes() {
    let b: u8 = kani::any();
    let case = any_case();
    let lower_or_digit = b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let upper = (b'A'..=b'F').contains(&b);
    match nibble_value(b, case) {
        Some(n) => {
            assert!(n < 16);
            assert!(lower_or_digit || (upper && case == HexCase::AnyCase));
            if lower_or_digit {
                assert_eq!(nibble_char(n), b);
            } else {
                assert_eq!(nibble_char(n), b.to_ascii_lowercase());
            }
        }
        None => assert!(!lower_or_digit && !(upper && case == HexCase::AnyCase)),
    }
}

/// `check_envelope(b).is_ok() => looks_like_pointer(b)` for every input of
/// 0..=48 arbitrary bytes (the version line is 44 bytes). `Pointer::parse`
/// returns early unless `check_envelope` succeeds, so this is the
/// "parse accepts => sniffer accepts" direction.
#[kani::proof]
#[kani::unwind(50)]
fn envelope_ok_implies_sniffer_le_48_bytes() {
    let len: usize = kani::any_where(|&l: &usize| l <= 48);
    let buf: [u8; 48] = kani::any();
    let bytes = &buf[..len];
    let ok = check_envelope(bytes).is_ok();
    kani::cover!(ok, "some input passes the envelope");
    if ok {
        assert!(looks_like_pointer(bytes));
    }
}

/// Canonicality of the size field, parse side: for every value of 0..=6
/// arbitrary bytes, if `parse_size` accepts it then the canonical decimal of
/// the result is the same bytes (so `+5`, `05`, `-0`, `5 ` are rejected).
#[kani::proof]
#[kani::unwind(8)]
fn size_parse_then_format_identity_le_6_bytes() {
    let len: usize = kani::any_where(|&l: &usize| l <= 6);
    let value: [u8; 6] = kani::any();
    let value = &value[..len];
    let parsed = parse_size(value);
    kani::cover!(parsed.is_some(), "some size parses");
    kani::cover!(
        parsed.is_none() && len > 0,
        "some non-empty size is rejected"
    );
    if let Some(n) = parsed {
        let mut out = Vec::new();
        push_decimal(&mut out, n);
        assert_eq!(out.as_slice(), value);
    }
}

/// Size field, write side, for sizes below 10^6: the canonical decimal of
/// `n` parses back to `n`. (Full-`u64` symbolic division did not finish in
/// 10 minutes; the `i64::MAX` boundary is covered by
/// `size_format_then_parse_near_i64_max` and the proptests.)
#[kani::proof]
#[kani::unwind(22)]
fn size_format_then_parse_identity_lt_1e6() {
    let n: u64 = kani::any_where(|&n: &u64| n < 1_000_000);
    let mut out = Vec::new();
    push_decimal(&mut out, n);
    assert_eq!(parse_size(&out), Some(n));
}

/// Size field at the `i64::MAX` boundary (+-255): accepted iff `<= i64::MAX`.
#[kani::proof]
#[kani::unwind(22)]
fn size_format_then_parse_near_i64_max() {
    let delta: u8 = kani::any();
    let up: bool = kani::any();
    let max = i64::MAX as u64;
    let n = if up {
        max + u64::from(delta)
    } else {
        max - u64::from(delta)
    };
    let mut out = Vec::new();
    push_decimal(&mut out, n);
    let expected = if n <= max { Some(n) } else { None };
    assert_eq!(parse_size(&out), expected);
}
