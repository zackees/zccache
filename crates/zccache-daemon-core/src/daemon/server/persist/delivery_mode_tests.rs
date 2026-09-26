//! Layer B (resolution) and layer C (tier planner) of the #1683 test design.

use super::*;
use MaterializationMode::{Auto, Copy, Link, Reflink};

fn caps(reflink: bool, hardlink: bool) -> VolumeCaps {
    VolumeCaps {
        reflink,
        hardlink,
        readonly_enforced: hardlink,
        file_id: FileIdWidth::Bits64,
        hardlink_limit: 1000,
    }
}

const FULL: (bool, bool) = (true, true);
const HL: (bool, bool) = (false, true);
const RL: (bool, bool) = (true, false);
const NONE: (bool, bool) = (false, false);
const AT_LIMIT: u64 = 1000;

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn request_env_beats_service_default() {
    let request = env(&[("ZCCACHE_MODE", "COPY")]);
    assert_eq!(resolve_request_mode(Some(&request), Some(Link)), Copy);
}

#[test]
fn service_default_applies_without_a_request_value() {
    assert_eq!(resolve_request_mode(None, Some(Reflink)), Reflink);
    let unrelated = env(&[("PATH", "/bin")]);
    assert_eq!(resolve_request_mode(Some(&unrelated), Some(Link)), Link);
}

#[test]
fn no_value_anywhere_is_auto() {
    assert_eq!(resolve_request_mode(None, None), Auto);
}

#[test]
fn empty_request_value_falls_through() {
    let request = env(&[("ZCCACHE_MODE", "")]);
    assert_eq!(resolve_request_mode(Some(&request), Some(Copy)), Copy);
}

#[test]
fn invalid_request_value_is_skipped_not_honored() {
    let request = env(&[("ZCCACHE_MODE", "hardlink")]);
    assert_eq!(resolve_request_mode(Some(&request), Some(Copy)), Copy);
    assert_eq!(resolve_request_mode(Some(&request), None), Auto);
}

#[test]
fn two_requests_with_different_modes_share_one_default() {
    let default = MaterializationModeDefault::new(Some(Link));
    let copy = env(&[("ZCCACHE_MODE", "copy")]);
    let reflink = env(&[("ZCCACHE_MODE", "reflink")]);
    assert_eq!(resolve_request_mode(Some(&copy), default.get()), Copy);
    assert_eq!(resolve_request_mode(Some(&reflink), default.get()), Reflink);
    assert_eq!(resolve_request_mode(None, default.get()), Link);
}

#[test]
fn service_default_round_trips_every_mode_and_unset() {
    let default = MaterializationModeDefault::new(None);
    assert_eq!(default.get(), None);
    for mode in MaterializationMode::ALL {
        default.set(Some(mode));
        assert_eq!(default.get(), Some(mode));
    }
    default.set(None);
    assert_eq!(default.get(), None);
}

/// (name, mode, policy-eligible, (reflink, hardlink) caps, link count,
/// expected reflink, expected hardlink)
type PlanRow = (
    &'static str,
    MaterializationMode,
    bool,
    (bool, bool),
    u64,
    bool,
    bool,
);

/// The #1683 truth table. `eligible` is whether the output's delivery policy
/// allows sharing an inode.
#[test]
fn plan_tiers_truth_table() {
    let rows: &[PlanRow] = &[
        ("AUTO eligible FULL", Auto, true, FULL, 1, true, true),
        ("AUTO eligible HL", Auto, true, HL, 1, false, true),
        (
            "AUTO eligible HL at limit",
            Auto,
            true,
            HL,
            AT_LIMIT,
            false,
            false,
        ),
        ("AUTO independent FULL", Auto, false, FULL, 1, true, false),
        ("AUTO eligible NONE", Auto, true, NONE, 1, false, false),
        ("AUTO independent NONE", Auto, false, NONE, 1, false, false),
        ("LINK eligible FULL", Link, true, FULL, 1, false, true),
        (
            "LINK independent FULL (demoted)",
            Link,
            false,
            FULL,
            1,
            true,
            false,
        ),
        ("LINK eligible RL", Link, true, RL, 1, false, false),
        (
            "LINK eligible HL at limit",
            Link,
            true,
            HL,
            AT_LIMIT,
            false,
            false,
        ),
        ("COPY eligible FULL", Copy, true, FULL, 1, false, false),
        ("COPY independent FULL", Copy, false, FULL, 1, false, false),
        ("REFLINK eligible FULL", Reflink, true, FULL, 1, true, false),
        ("REFLINK independent RL", Reflink, false, RL, 1, true, false),
        ("REFLINK eligible HL", Reflink, true, HL, 1, false, false),
        (
            "REFLINK eligible NONE",
            Reflink,
            true,
            NONE,
            1,
            false,
            false,
        ),
    ];
    for &(name, mode, eligible, (reflink, hardlink), links, want_reflink, want_hardlink) in rows {
        let plan = plan_tiers(mode, eligible, caps(reflink, hardlink), links);
        assert_eq!(
            plan,
            TierPlan {
                reflink: want_reflink,
                hardlink: want_hardlink
            },
            "{name}"
        );
    }
}

/// Every mode x eligibility x capability x link-count combination.
fn every_case() -> impl Iterator<Item = (MaterializationMode, bool, VolumeCaps, u64, TierPlan)> {
    MaterializationMode::ALL.into_iter().flat_map(|mode| {
        [true, false].into_iter().flat_map(move |eligible| {
            [FULL, HL, RL, NONE]
                .into_iter()
                .flat_map(move |(reflink, hardlink)| {
                    [0, AT_LIMIT - 1, AT_LIMIT].into_iter().map(move |links| {
                        let caps = caps(reflink, hardlink);
                        (
                            mode,
                            eligible,
                            caps,
                            links,
                            plan_tiers(mode, eligible, caps, links),
                        )
                    })
                })
        })
    })
}

#[test]
fn plan_never_hardlinks_an_output_its_policy_keeps_independent() {
    for (mode, eligible, _, _, plan) in every_case() {
        if plan.hardlink {
            assert!(eligible, "{mode}: hardlinked an independent-only output");
            assert!(matches!(mode, Auto | Link), "{mode} hardlinked");
        }
    }
}

#[test]
fn plan_copy_mode_is_copy_only() {
    for (mode, _, _, _, plan) in every_case() {
        if mode == Copy {
            assert_eq!(
                plan,
                TierPlan {
                    reflink: false,
                    hardlink: false
                }
            );
        }
    }
}

#[test]
fn plan_reflink_mode_output_is_always_independent() {
    for (mode, _, _, _, plan) in every_case() {
        if mode == Reflink {
            assert!(!plan.hardlink);
        }
    }
}

#[test]
fn plan_respects_probed_capabilities_and_link_limit() {
    for (mode, _, caps, links, plan) in every_case() {
        assert!(
            !plan.reflink || caps.reflink,
            "{mode}: reflink without capability"
        );
        assert!(
            !plan.hardlink || caps.hardlink,
            "{mode}: hardlink without capability"
        );
        assert!(
            !plan.hardlink || links < caps.hardlink_limit,
            "{mode}: past link limit"
        );
    }
}

/// The executor plans the reflink tier before reading the cache file's link
/// count, so the reflink decision must not depend on it.
#[test]
fn plan_reflink_decision_ignores_the_link_count() {
    for (mode, eligible, caps, links, plan) in every_case() {
        assert_eq!(
            plan.reflink,
            plan_tiers(mode, eligible, caps, 0).reflink,
            "{mode} at {links} links"
        );
    }
}

#[test]
fn hardlink_permission_matches_the_plan_without_a_probe() {
    for (mode, eligible, caps, links, plan) in every_case() {
        if caps.hardlink && links < caps.hardlink_limit {
            assert_eq!(plan.hardlink, hardlink_permitted(mode, eligible), "{mode}");
        }
    }
}

/// An explicit REFLINK overrides the legacy `ZCCACHE_DISABLE_REFLINK`
/// switch; AUTO and LINK keep honoring it; COPY never uses probed caps
/// (#1683 decision D3).
#[test]
fn legacy_disable_reflink_applies_to_every_mode_but_explicit_reflink() {
    let probed = caps(true, true);
    for mode in MaterializationMode::ALL {
        let switched = caps_for_mode(mode, probed, true);
        assert_eq!(
            switched.reflink,
            mode == Reflink,
            "{mode} with legacy switch"
        );
        assert_eq!(
            switched.hardlink,
            mode != Copy,
            "{mode} hardlink capability"
        );
        let untouched = caps_for_mode(mode, probed, false);
        assert_eq!(
            untouched.reflink,
            mode != Copy,
            "{mode} without legacy switch"
        );
    }
}

#[test]
fn copy_mode_plans_against_copy_only_caps_without_probing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache.bin");
    std::fs::write(&cache, b"x").unwrap();
    let caps = delivery_caps(Copy, &cache, &dir.path().join("out.bin"));
    assert!(!caps.reflink && !caps.hardlink);
    assert_eq!(probes_under(dir.path()), 0);
}
