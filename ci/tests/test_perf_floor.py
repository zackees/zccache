"""#1445: fixed-overhead cold floors must not depend on the runner's speed."""

import pytest

from ci import benchmark_stats, perf_floor, perf_guard

C_REF = perf_floor.REFERENCE_BARE_SECONDS[("c-inline", "Single-file, Cold", "bare")]
RUST_REF = perf_floor.REFERENCE_BARE_SECONDS[("rust", "Check, Cold", "bare")]


def _c_log(bare: float, zccache: float) -> str:
    return f"""
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | {bare:.3f}s | {zccache * 1.2:.3f}s | {zccache:.3f}s | 1.2x faster | 1.0x slower |
| Single-file, Warm | {bare:.3f}s | 2.000s | **0.100s** | **20x faster** | **20x faster** |
"""


def _c_cold_bare(
    log: str, **kwargs
) -> tuple[perf_guard.GuardReport, perf_guard.ScenarioStatus]:
    report = perf_guard.evaluate_attempts(
        [benchmark_stats.parse_benchmark_log(log)],
        languages=("c",),
        cold_bare_threshold=0.80,
        **kwargs,
    )
    (status,) = [
        s
        for s in report.statuses
        if s.scenario == "Single-file, Cold" and s.baseline == "bare"
    ]
    return report, status


def test_floor_is_unchanged_on_the_calibration_runner_and_for_unlisted_checks() -> None:
    assert perf_floor.effective_floor(0.80, C_REF, C_REF) == 0.80
    assert perf_floor.effective_floor(0.80, C_REF, C_REF + 0.5) == 0.80
    assert perf_floor.effective_floor(0.80, None, 1.0) == 0.80
    # Warm floors are speedups, not overhead budgets; never re-based.
    assert perf_floor.effective_floor(1.5, C_REF, 0.5) == 1.5
    assert (
        perf_floor.reference_bare_seconds("c-inline", "Single-file, Warm", "bare")
        is None
    )
    assert (
        perf_floor.reference_bare_seconds("c-inline", "Single-file, Cold", "sccache")
        is None
    )


def test_fast_runner_keeps_the_reference_added_time_budget() -> None:
    floor = perf_floor.effective_floor(0.80, C_REF, 1.3)
    budget = (1 / 0.80 - 1) * C_REF
    assert floor == pytest.approx(1.3 / (1.3 + budget))
    assert floor < 0.80


# Best attempts of every C cold-row failure on `main` since 2026-09-23, all on the
# fast runner class (bare seconds, zccache seconds, run id).
FAST_RUNNER_FAILURES = [
    (1.301, 1.680, 36221436019),
    (1.742, 2.179, 36220411240),
    (1.804, 2.271, 36190893054),
    (1.317, 1.723, 36082224556),
    (1.217, 1.674, 35816084713),
]


@pytest.mark.parametrize(("bare", "zccache", "run_id"), FAST_RUNNER_FAILURES)
def test_unchanged_tree_fast_runner_samples_pass(
    bare: float, zccache: float, run_id: int
) -> None:
    report, status = _c_cold_bare(_c_log(bare, zccache))

    assert bare / zccache < 0.80, run_id  # red under the plain ratio floor
    assert report.status_passed(status), run_id
    check = perf_guard._format_status_check(report, status)
    assert "0.80x floor re-based" in check
    assert f"{status.floor_for(status.samples[0]):.2f}x" in check


@pytest.mark.parametrize("bare", [1.2, 1.6, C_REF, 2.5])
def test_doubled_zccache_overhead_fails_on_every_runner_class(bare: float) -> None:
    typical_overhead = 0.48  # median added seconds over the same 60 runs
    _, status = _c_cold_bare(_c_log(bare, bare + 2 * typical_overhead))

    assert not status.passed


def test_calibration_runner_verdict_is_exactly_the_configured_floor() -> None:
    _, at_floor = _c_cold_bare(_c_log(2.4, 2.4 / 0.80))
    _, below = _c_cold_bare(_c_log(2.4, 2.4 / 0.79))

    assert at_floor.passed
    assert not below.passed


def test_distribution_mode_holds_every_attempt_to_its_own_floor() -> None:
    attempts = [
        benchmark_stats.parse_benchmark_log(
            _c_log(1.3, 1.7)
        ),  # fast runner, re-based pass
        benchmark_stats.parse_benchmark_log(
            _c_log(2.4, 3.2)
        ),  # 0.75x on the reference class
    ]
    report = perf_guard.evaluate_attempts(
        attempts,
        languages=("c",),
        cold_bare_threshold=0.80,
        require_all_attempts=True,
    )
    (status,) = [
        s
        for s in report.statuses
        if s.scenario == "Single-file, Cold" and s.baseline == "bare"
    ]

    assert status.passed
    assert not report.status_passed(status)
    assert perf_guard._selected_sample(report, status).attempt == 2


def test_report_json_records_the_floor_each_sample_was_held_to() -> None:
    report, _ = _c_cold_bare(_c_log(1.3, 1.7))
    payload = perf_guard.format_report_json(
        report, 1.5, 1.5, ("c",), cold_bare_threshold=0.80
    )
    (status,) = [
        s
        for s in payload["statuses"]
        if s["scenario"] == "Single-file, Cold" and s["baseline"] == "bare"
    ]

    assert status["reference_baseline_seconds"] == C_REF
    assert status["samples"][0]["floor"] == pytest.approx(
        perf_floor.effective_floor(0.80, C_REF, 1.3)
    )


def test_rust_check_cold_is_rebased_against_its_own_reference() -> None:
    assert 0.9 < RUST_REF < 1.5
    assert perf_floor.effective_floor(0.75, RUST_REF, RUST_REF) == 0.75
    assert perf_floor.effective_floor(0.75, RUST_REF, 0.9) < 0.75


# #1773: release-profile warm floor ratchet.


def test_warm_ratio_floor_is_none_for_unsampled_rows() -> None:
    assert (
        perf_floor.warm_ratio_floor("emscripten", "Single-file, Warm", "bare") is None
    )


def test_warm_ratio_floor_covers_every_row_perf_guard_evaluates() -> None:
    # Every (benchmark, scenario, baseline) pair the workflow's three
    # required-language matrix legs measure in warm mode must have a
    # ratcheted floor, or the release-profile headroom regresses silently
    # back to the pre-#1773 1.5x default for that row.
    expected_rows = {
        ("c-inline", "Single-file, Warm", "bare"),
        ("c-inline", "Single-file, Warm", "sccache"),
        ("c-static-library-link", "Static archive, Warm", "bare"),
        ("c-static-library-link", "Static archive, Warm", "sccache"),
        ("cpp-driver-link", "Driver link, Warm", "bare"),
        ("cpp-driver-link", "Driver link, Warm", "sccache"),
        ("cpp-inline", "Multi-file, Warm", "bare"),
        ("cpp-inline", "Multi-file, Warm", "sccache"),
        ("cpp-inline", "Single-file, Warm", "bare"),
        ("cpp-inline", "Single-file, Warm", "sccache"),
        ("cpp-response-file", "Multi-file RSP, Warm", "bare"),
        ("cpp-response-file", "Multi-file RSP, Warm", "sccache"),
        ("cpp-response-file", "Single-file RSP, Warm", "bare"),
        ("cpp-response-file", "Single-file RSP, Warm", "sccache"),
        ("cpp-sibling-remap", "Sibling-workspace no __FILE__, Warm", "bare"),
        ("cpp-sibling-remap", "Sibling-workspace no __FILE__, Warm", "sccache"),
        ("cpp-sibling-remap", "Sibling-workspace with __FILE__, Warm", "bare"),
        ("cpp-sibling-remap", "Sibling-workspace with __FILE__, Warm", "sccache"),
        ("rust", "Build, Warm", "bare"),
        ("rust", "Build, Warm", "sccache"),
        ("rust", "Check, Warm", "bare"),
        ("rust", "Check, Warm", "sccache"),
        ("rust-sibling-remap", "Sibling-workspace, Warm", "bare"),
        ("rust-sibling-remap", "Sibling-workspace, Warm", "sccache"),
        ("rust-workspace-link", "Workspace staticlib link, Warm", "bare"),
        ("rust-workspace-link", "Workspace staticlib link, Warm", "sccache"),
    }
    assert set(perf_floor.WARM_RATIO_FLOORS) == expected_rows
    for row, floor in perf_floor.WARM_RATIO_FLOORS.items():
        assert floor > perf_guard.DEFAULT_WARM_BARE_THRESHOLD, row


def test_dev_profile_level_warm_ratio_now_fails_the_ratchet() -> None:
    # #1767's own evidence: dev-profile C warm hits landed around 20-30x
    # (0.104s/0.099s zccache vs ~2.1-2.3s bare on this fixture size). That
    # ratio used to clear the flat 1.5x floor; it must not clear the
    # release-profile ratchet, which is what makes the ratchet meaningful.
    floor = perf_floor.warm_ratio_floor("c-inline", "Single-file, Warm", "bare")
    dev_profile_ratio = 2.2 / 0.104
    assert dev_profile_ratio < floor
