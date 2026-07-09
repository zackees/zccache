//! Exit-code conversion tests for [`super::super::util::exit_code_from_i32`].
//!
//! These guard the i32 → u8 truncation logic — a zero low-byte from a
//! non-zero source (e.g. 256) must stay a failure exit, not silently
//! collapse to success.

use std::process::ExitCode;

use super::super::util::exit_code_from_i32;

#[test]
fn exit_code_zero_stays_zero() {
    assert_eq!(exit_code_from_i32(0), ExitCode::from(0));
}

#[test]
fn exit_code_one_stays_one() {
    assert_eq!(exit_code_from_i32(1), ExitCode::from(1));
}

#[test]
fn exit_code_255_stays_255() {
    assert_eq!(exit_code_from_i32(255), ExitCode::from(255));
}

#[test]
fn exit_code_256_becomes_one_not_zero() {
    // Without the fix, 256 as u8 == 0, masking the failure.
    assert_ne!(exit_code_from_i32(256), ExitCode::from(0));
    assert_eq!(exit_code_from_i32(256), ExitCode::from(1));
}

#[test]
fn exit_code_512_becomes_one_not_zero() {
    assert_eq!(exit_code_from_i32(512), ExitCode::from(1));
}

#[test]
fn exit_code_negative_preserves_failure() {
    // -1 & 0xFF == 255
    assert_ne!(exit_code_from_i32(-1), ExitCode::from(0));
    assert_eq!(exit_code_from_i32(-1), ExitCode::from(255));
}

#[test]
fn exit_code_257_keeps_low_byte() {
    // 257 & 0xFF == 1, non-zero, so kept as-is.
    assert_eq!(exit_code_from_i32(257), ExitCode::from(1));
}
