"""Runner-invariant ratio floors for fixed-overhead cold checks (#1445).

A cold miss costs the bare compile plus zccache's own work (dispatch, include
scan, hashing, store). That added cost is roughly constant in wall-clock
terms, not a fraction of the compile, so the `bare / zccache` ratio depends on
how fast the runner's compiler is: the same overhead `o` over a bare time `b`
gives `b / (b + o)`, which falls as `b` falls.

Hosted `ubuntu-latest` jobs land on two runner classes. On the class the
floors were calibrated on, the 50-file C cold row's bare run takes about 2.2s;
on the faster class it takes 1.1-1.8s. Over 60 `main` runs (2026-09-17..26)
zccache's added cold time stayed in the same 0.3-0.55s band on both, yet every
C cold-row failure since 2026-09-23 was on the fast class, where a 0.80x floor
leaves ~0.3s for that band. The verdict came from the hardware lottery.

For a check listed in `REFERENCE_BARE_SECONDS`, the floor keeps the absolute
meaning it had on the calibration runner: the allowed added time is
`(1 / floor - 1) * reference`. A runner at or slower than the reference gets
exactly the configured floor; a faster runner is held to the same added-time
budget, expressed as a lower ratio. A real regression (the added time growing)
fails on both classes as it does today on the calibration class.
"""

from __future__ import annotations

# (benchmark id, scenario, baseline) -> median bare seconds on the runner class
# the floor was calibrated on. Values are the median bare time of the check's
# passing `main` runs, 2026-09-17..26 (see #1445 for the per-run data).
REFERENCE_BARE_SECONDS: dict[tuple[str, str, str], float] = {
    ("c-inline", "Single-file, Cold", "bare"): 2.17,
    ("rust", "Check, Cold", "bare"): 1.29,
}


def reference_bare_seconds(
    benchmark: str, scenario: str, baseline: str
) -> float | None:
    """The calibration-runner bare time for a check, or None if it is not re-based."""
    return REFERENCE_BARE_SECONDS.get((benchmark, scenario, baseline))


def effective_floor(
    threshold: float,
    reference_seconds: float | None,
    baseline_seconds: float | None,
) -> float:
    """Ratio floor for one sample measured with `baseline_seconds` of bare time.

    Identical to `threshold` unless the check is re-based and this runner's
    bare run beat the reference, in which case the floor is lowered only as far
    as keeps the reference runner's added-time budget.
    """
    if (
        reference_seconds is None
        or baseline_seconds is None
        or baseline_seconds <= 0
        or not 0 < threshold < 1
        or baseline_seconds >= reference_seconds
    ):
        return threshold
    allowed_added_seconds = (1 / threshold - 1) * reference_seconds
    return baseline_seconds / (baseline_seconds + allowed_added_seconds)


# #1773: warm-row ratio floors, ratcheted against release-profile Perf Guard
# samples collected after #1767 switched the hosted daemon build from dev to
# release profile.
#
# Every hosted `main`-push Perf Guard run that actually reached the
# `perf-guard` matrix job (the job that exercises the benchmark rows) is
# gated behind the `Cache pre-prune barrier`; most pushes in this window
# failed that unrelated barrier before the benchmark job ever started (#1758,
# fixed by #1781). Three release-profile runs produced full benchmark data in
# the window after #1767 merged (2026-09-28T21:14:21Z):
#
#   - 36478788423 (workflow_dispatch on the #1767 branch, same code as main
#     post-merge; cited in #1767's own PR body as "the first release-profile
#     hosted run")
#   - 36493296576 (main push)
#   - 36504459992 (main push)
#
# That is 3 runs (3-4 samples per row: `rust` rows retried an extra internal
# attempt), short of the >=10 the issue asks for -- there simply were not 10
# green hosted runs in the available window. Recorded honestly here and in
# `ci/perf_threshold_history.json`; collecting more samples as `main` produces
# them is tracked as a follow-up under #1779/#1778.
#
# Each floor below is `min(observed warm ratio) / 1.2` -- 20% headroom under
# the worst sample seen, on the same "min or percentile times a margin"
# convention `effective_floor` above and the existing per-scenario overrides
# in `ci/perf_guard.py` already use. Warm ratios only (cold floors already
# have runner-class rebasing via `REFERENCE_BARE_SECONDS` above, and their
# release-profile shift was mild -- 0.73-0.77x dev to 0.83-0.84x release --
# so they are left as-is).
#
# (benchmark, scenario, baseline) -> floor. See ci/perf_threshold_history.json
# for the full per-row sample table (n, min/median/max, margin, run ids).
WARM_RATIO_FLOORS: dict[tuple[str, str, str], float] = {
    ("c-inline", "Single-file, Warm", "bare"): 24.35,
    ("c-inline", "Single-file, Warm", "sccache"): 2.85,
    ("c-static-library-link", "Static archive, Warm", "bare"): 117.5,
    ("c-static-library-link", "Static archive, Warm", "sccache"): 115.8,
    ("cpp-driver-link", "Driver link, Warm", "bare"): 134.1,
    ("cpp-driver-link", "Driver link, Warm", "sccache"): 143.3,
    ("cpp-inline", "Multi-file, Warm", "bare"): 581.3,
    ("cpp-inline", "Multi-file, Warm", "sccache"): 577.2,
    ("cpp-inline", "Single-file, Warm", "bare"): 104.3,
    ("cpp-inline", "Single-file, Warm", "sccache"): 3.06,
    ("cpp-response-file", "Multi-file RSP, Warm", "bare"): 411.2,
    ("cpp-response-file", "Multi-file RSP, Warm", "sccache"): 408.5,
    ("cpp-response-file", "Single-file RSP, Warm", "bare"): 79.1,
    ("cpp-response-file", "Single-file RSP, Warm", "sccache"): 2.34,
    ("cpp-sibling-remap", "Sibling-workspace no __FILE__, Warm", "bare"): 93.5,
    ("cpp-sibling-remap", "Sibling-workspace no __FILE__, Warm", "sccache"): 2.79,
    ("cpp-sibling-remap", "Sibling-workspace with __FILE__, Warm", "bare"): 92.8,
    ("cpp-sibling-remap", "Sibling-workspace with __FILE__, Warm", "sccache"): 122.4,
    ("rust", "Build, Warm", "bare"): 9.69,
    ("rust", "Build, Warm", "sccache"): 12.2,
    ("rust", "Check, Warm", "bare"): 6.72,
    ("rust", "Check, Warm", "sccache"): 9.98,
    ("rust-sibling-remap", "Sibling-workspace, Warm", "bare"): 10.2,
    ("rust-sibling-remap", "Sibling-workspace, Warm", "sccache"): 12.39,
    ("rust-workspace-link", "Workspace staticlib link, Warm", "bare"): 3.25,
    ("rust-workspace-link", "Workspace staticlib link, Warm", "sccache"): 4.09,
}


def warm_ratio_floor(benchmark: str, scenario: str, baseline: str) -> float | None:
    """The ratcheted release-profile warm floor for a row, or None if unlisted.

    Falls back to the caller's existing default (currently 1.5x) for any row
    not in `WARM_RATIO_FLOORS` -- e.g. emscripten warm rows, which this
    ratchet has no hosted release-profile samples for.
    """
    return WARM_RATIO_FLOORS.get((benchmark, scenario, baseline))
