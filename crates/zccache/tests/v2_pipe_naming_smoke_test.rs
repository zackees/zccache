//! Downstream-consumer smoke test for `running_process::broker::lifecycle::names_v2`.
//!
//! Tracks the long-form migration in #777. Slice 3b of upstream #483
//! (running-process PR #487) added a v2 pipe-naming utility that
//! constructs `rpb-v2-<program>-<sid_hash>-<pipe_idx>` names — the
//! namespace a v2 broker will bind on once the binary slices land.
//!
//! Asserting on the exact string shape here pins the name format from
//! a downstream consumer so that any future drift upstream surfaces
//! immediately, before zccache's runtime broker integration starts
//! depending on the bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use running_process::broker::lifecycle::names_v2::v2_program_pipe;

#[test]
fn v2_program_pipe_produces_canonical_zccache_pipe_name() {
    let name = v2_program_pipe("zccache", "deadbeefcafef00d", 0)
        .expect("zccache + 16-hex sid + idx 0 are valid inputs");
    assert_eq!(name, "rpb-v2-zccache-deadbeefcafef00d-0");
}

#[test]
fn v2_program_pipe_distinct_pipe_idx_distinct_names() {
    let name_0 = v2_program_pipe("zccache", "deadbeefcafef00d", 0).expect("idx=0 valid");
    let name_7 = v2_program_pipe("zccache", "deadbeefcafef00d", 7).expect("idx=7 valid");

    assert_ne!(name_0, name_7);
    assert!(name_7.ends_with("-7"));
}

#[test]
fn v2_program_pipe_rejects_invalid_program() {
    assert!(v2_program_pipe("ZCCACHE", "deadbeefcafef00d", 0).is_err());
}

/// P1-6 from #848: a sid_hash with non-16-char length must be rejected
/// by the v2 namer. Pins the contract from the consumer side so a
/// future upstream relaxation (e.g. accepting 32-char hashes for a
/// stronger SID derivation) trips this test and forces a coordinated
/// update of the zccache pin format.
#[test]
fn v2_program_pipe_rejects_invalid_sid_length() {
    // Too short.
    assert!(
        v2_program_pipe("zccache", "deadbeef", 0).is_err(),
        "8-char sid (half-length) must be rejected"
    );
    // Too long.
    assert!(
        v2_program_pipe("zccache", "deadbeefcafef00ddeadbeefcafef00d", 0).is_err(),
        "32-char sid (double-length) must be rejected"
    );
    // Empty.
    assert!(
        v2_program_pipe("zccache", "", 0).is_err(),
        "empty sid must be rejected"
    );
}

/// P1-6 from #848: non-hex chars in the sid slot must be rejected. The
/// v2 namer carries the sid through to a wire-visible identifier; a
/// non-hex byte would surface as a corrupt pipe name on Windows or a
/// confusing UDS path on Unix. Forward-compat: also document that
/// uppercase hex is treated the same way as the upstream lowercasing
/// convention.
#[test]
fn v2_program_pipe_rejects_non_hex_sid() {
    assert!(
        v2_program_pipe("zccache", "deadbeefcafef00g", 0).is_err(),
        "non-hex char 'g' in sid must be rejected"
    );
    assert!(
        v2_program_pipe("zccache", "deadbeefcafef00!", 0).is_err(),
        "non-hex punctuation in sid must be rejected"
    );
}

/// P1-6 from #848: the `pipe_idx = u32::MAX` edge case must serialize
/// without overflow into the final pipe name string. Documents the
/// canonical numeric format (decimal, no zero-padding) so a future
/// upstream change to hex/padded encoding fails this test and forces
/// downstream consumers to update in lockstep.
#[test]
fn v2_program_pipe_handles_pipe_idx_max() {
    let name = v2_program_pipe("zccache", "deadbeefcafef00d", u32::MAX)
        .expect("u32::MAX pipe_idx must serialize without error");
    assert!(
        name.ends_with(&format!("-{}", u32::MAX)),
        "u32::MAX must serialize as decimal at the end of the name: {name}"
    );
    // u32::MAX is 4_294_967_295 — 10 decimal digits. Confirm no
    // padding/truncation was applied.
    assert!(
        name.ends_with("-4294967295"),
        "exact decimal expected; got: {name}"
    );
}
