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
