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

use running_process::broker::lifecycle::names_v2::v2_program_pipe;

#[test]
fn v2_program_pipe_produces_canonical_zccache_pipe_name() {
    let name = v2_program_pipe("zccache", "deadbeefcafef00d", 0)
        .expect("zccache + 16-hex sid + idx 0 are valid inputs");
    assert_eq!(name, "rpb-v2-zccache-deadbeefcafef00d-0");
}

#[test]
fn v2_program_pipe_distinct_pipe_idx_distinct_names() {
    let name_0 = v2_program_pipe("zccache", "deadbeefcafef00d", 0)
        .expect("idx=0 valid");
    let name_7 = v2_program_pipe("zccache", "deadbeefcafef00d", 7)
        .expect("idx=7 valid");

    assert_ne!(name_0, name_7);
    assert!(name_7.ends_with("-7"));
}

#[test]
fn v2_program_pipe_rejects_invalid_program() {
    assert!(v2_program_pipe("ZCCACHE", "deadbeefcafef00d", 0).is_err());
}
