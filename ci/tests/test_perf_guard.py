import json
from pathlib import Path

from ci import benchmark_stats, perf_guard, perf_watchdog


PASSING_LOG = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | 3.200s | 2.000s | 1.6x faster | 1.5x faster |
| Single-file, Warm | 3.000s | 2.000s | **1.000s** | **2.0x faster** | **3.0x faster** |

## Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 6.000s | 7.000s | 4.000s | 1.8x faster | 1.5x faster |
| Single-file, Warm | 6.000s | 1.500s | **0.050s** | **30x faster** | **120x faster** |

## C++ Sibling-Workspace Remap Benchmark: 50 .cpp files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Sibling-workspace no __FILE__, Warm | 3.000s | 1.000s | **1.000s** | **1.0x faster** | **3.0x faster** |
| Sibling-workspace with __FILE__, Warm | 3.000s | 1.500s | **1.000s** | **1.5x faster** | **3.0x faster** |

## Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 9.000s | 10.000s | 6.000s | 1.7x faster | 1.5x faster |
| Build, Warm | 9.000s | 8.000s | **0.100s** | **80x faster** | **90x faster** |
"""


FAILING_RUST_LOG = PASSING_LOG.replace(
    "| Build, Warm | 9.000s | 8.000s | **0.100s** | **80x faster** | **90x faster** |",
    "| Build, Warm | 1.400s | 1.200s | **1.000s** | **1.2x faster** | **1.4x faster** |",
)

FAILING_SCCACHE_LOG = PASSING_LOG.replace(
    "| Single-file, Warm | 3.000s | 2.000s | **1.000s** | **2.0x faster** | **3.0x faster** |",
    "| Single-file, Warm | 3.000s | 1.200s | **1.000s** | **1.2x faster** | **3.0x faster** |",
)


MISSING_C_LOG = """
## Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 6.000s | 7.000s | 4.000s | 1.8x faster | 1.5x faster |
| Single-file, Warm | 6.000s | 1.500s | **0.050s** | **30x faster** | **120x faster** |

## Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 9.000s | 10.000s | 6.000s | 1.7x faster | 1.5x faster |
| Build, Warm | 9.000s | 8.000s | **0.100s** | **80x faster** | **90x faster** |
"""


NEAR_BARE_COLD_C_LOG = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | 4.000s | 3.300s | 1.2x faster | 1.1x slower |
| Single-file, Warm | 3.000s | 2.000s | **0.100s** | **20x faster** | **30x faster** |
"""


def rows(log: str):
    return benchmark_stats.parse_benchmark_log(log)


def test_exact_threshold_passes():
    report = perf_guard.evaluate_attempts([rows(PASSING_LOG)], threshold=1.5)

    assert report.passed
    assert not report.missing_requirements


def test_below_threshold_fails():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)

    assert not report.passed
    failing = [status for status in report.statuses if not status.passed]
    assert [(status.language, status.scenario, status.baseline) for status in failing] == [
        ("rust", "Build, Warm", "bare"),
        ("rust", "Build, Warm", "sccache"),
    ]


def test_sccache_threshold_is_enforced_separately_from_bare():
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    assert not report.passed
    failing = [status for status in report.statuses if not status.passed]
    assert [(status.language, status.scenario, status.baseline) for status in failing] == [
        ("c", "Single-file, Warm", "sccache")
    ]


def test_default_cold_thresholds_allow_near_bare_misses():
    report = perf_guard.evaluate_attempts(
        [rows(NEAR_BARE_COLD_C_LOG)],
        languages=("c",),
    )

    assert report.passed
    cold_statuses = [status for status in report.statuses if status.mode == "cold"]
    assert [(status.baseline, status.threshold) for status in cold_statuses] == [
        ("bare", 0.85),
        ("sccache", 1.0),
    ]


C_STATIC_LIBRARY_LINK_LOG = """
## C Static-Library Link Benchmark: 50 .o inputs, 5 warm trials

| Scenario | Bare ar | sccache | zccache | bare cache | sccache cache | zccache cache | vs sccache | vs Bare ar |
|:---------|----------:|--------:|--------:|-----------:|--------------:|--------------:|-----------:|--------------:|
| Static archive, Cold | 0.058s | 0.055s | 0.070s | 0 B | 0 B | 189.5 KiB | 1.3x slower | 1.2x slower |
| Static archive, Warm | 0.056s | 0.056s | **0.001s** | 0 B | 0 B | 193.7 KiB | **56x faster** | **56x faster** |
"""


def test_c_static_library_link_cold_uses_dedicated_threshold_and_passes_baseline():
    # The cold archive path is dominated by fixed daemon/hash overhead on a
    # tiny ~60 ms `ar` run. Guard warm-cache speedups tightly, but use a
    # scenario floor for cold mode so routine runner noise does not fail main.
    report = perf_guard.evaluate_attempts(
        [rows(C_STATIC_LIBRARY_LINK_LOG)],
        languages=("c",),
        require_coverage=False,
    )

    cold_statuses = [
        status for status in report.statuses if status.scenario == "Static archive, Cold"
    ]
    assert {
        (status.baseline, status.best_ratio, status.threshold) for status in cold_statuses
    } == {
        ("bare", 0.829, perf_guard.C_STATIC_LIBRARY_LINK_COLD_BARE_THRESHOLD),
        ("sccache", 0.786, perf_guard.C_STATIC_LIBRARY_LINK_COLD_SCCACHE_THRESHOLD),
    }
    assert all(status.passed for status in cold_statuses), [
        (s.baseline, s.best_ratio, s.threshold) for s in cold_statuses
    ]


def test_c_static_library_link_cold_regression_below_threshold_fails():
    regressed = C_STATIC_LIBRARY_LINK_LOG.replace(
        "| Static archive, Cold | 0.058s | 0.055s | 0.070s | 0 B | 0 B | 189.5 KiB | 1.3x slower | 1.2x slower |",
        "| Static archive, Cold | 0.058s | 0.055s | 0.090s | 0 B | 0 B | 189.5 KiB | 1.6x slower | 1.6x slower |",
    )
    report = perf_guard.evaluate_attempts(
        [rows(regressed)],
        languages=("c",),
        require_coverage=False,
    )

    failing_baselines = {
        status.baseline
        for status in report.statuses
        if status.scenario == "Static archive, Cold" and not status.passed
    }
    assert failing_baselines == {"bare", "sccache"}


def test_cold_floor_overrides_are_targeted():
    cpp_log = PASSING_LOG.replace(
        "| Single-file, Cold | 6.000s | 7.000s | 4.000s | 1.8x faster | 1.5x faster |",
        "| Single-file, Cold | 6.000s | 3.800s | 4.000s | 1.1x slower | 1.5x faster |",
    )
    cpp_report = perf_guard.evaluate_attempts([rows(cpp_log)], languages=("c++",))

    assert cpp_report.passed
    cpp_cold_sccache = [
        status
        for status in cpp_report.statuses
        if status.scenario == "Single-file, Cold" and status.baseline == "sccache"
    ][0]
    assert cpp_cold_sccache.best_ratio == 0.95
    assert cpp_cold_sccache.threshold == 0.9

    rust_log = PASSING_LOG.replace(
        "| Build, Cold | 9.000s | 10.000s | 6.000s | 1.7x faster | 1.5x faster |",
        "| Build, Cold | 1.894s | 2.678s | 4.142s | 1.5x slower | 2.2x slower |",
    )
    rust_report = perf_guard.evaluate_attempts([rows(rust_log)], languages=("rust",))

    assert rust_report.passed
    rust_cold = [
        status
        for status in rust_report.statuses
        if status.scenario == "Build, Cold"
    ]
    assert {
        (status.baseline, status.best_ratio, status.threshold)
        for status in rust_cold
    } == {
        ("bare", 0.457, 0.4),
        ("sccache", 0.647, 0.6),
    }

    rust_warm_report = perf_guard.evaluate_attempts(
        [rows(FAILING_RUST_LOG)],
        languages=("rust",),
        threshold=1.5,
    )

    assert not rust_warm_report.passed
    rust_warm_sccache = [
        status
        for status in rust_warm_report.statuses
        if status.scenario == "Build, Warm" and status.baseline == "sccache"
    ][0]
    assert rust_warm_sccache.best_ratio == 1.2
    assert rust_warm_sccache.threshold == 1.5


RUST_WORKSPACE_LINK_LOG = """
## Rust Workspace Link Benchmark: 50 .rlib inputs, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Workspace staticlib link, Cold | 0.036s | 0.128s | 0.127s | ~same | 3.5x slower |
| Workspace staticlib link, Warm | 0.035s | 0.038s | **0.001s** | **38x faster** | **35x faster** |
"""


def test_rust_workspace_link_cold_uses_dedicated_threshold_and_passes_baseline():
    # Regression guard for issue #517: rust-workspace-link Cold ran at 0.283x
    # of bare on the 2026-05-31 perf baseline (127 ms vs 36 ms). The default
    # cold-bare floor (0.85) would FAIL that row and force a noisy ratchet on
    # every cluster run, so the scenario gets a dedicated floor of 0.20 that
    # passes today and fires on a real backslide.
    report = perf_guard.evaluate_attempts(
        [rows(RUST_WORKSPACE_LINK_LOG)],
        languages=("rust",),
        require_coverage=False,
    )

    cold_statuses = [
        status
        for status in report.statuses
        if status.scenario == "Workspace staticlib link, Cold"
    ]
    assert {
        (status.baseline, status.threshold) for status in cold_statuses
    } == {
        ("bare", perf_guard.RUST_WORKSPACE_LINK_COLD_BARE_THRESHOLD),
        ("sccache", perf_guard.RUST_WORKSPACE_LINK_COLD_SCCACHE_THRESHOLD),
    }
    assert all(status.passed for status in cold_statuses), [
        (s.baseline, s.best_ratio, s.threshold) for s in cold_statuses
    ]


def test_rust_workspace_link_cold_regression_below_threshold_fails():
    # Doubling the cold time (127 ms → 254 ms) drops the ratio from 0.283 to
    # ~0.14, well below the 0.20 floor — the gate must fire.
    regressed = RUST_WORKSPACE_LINK_LOG.replace(
        "| Workspace staticlib link, Cold | 0.036s | 0.128s | 0.127s | ~same | 3.5x slower |",
        "| Workspace staticlib link, Cold | 0.036s | 0.128s | 0.254s | 1.0x slower | 7.0x slower |",
    )
    report = perf_guard.evaluate_attempts(
        [rows(regressed)],
        languages=("rust",),
        require_coverage=False,
    )

    failing_baselines = {
        status.baseline
        for status in report.statuses
        if status.scenario == "Workspace staticlib link, Cold" and not status.passed
    }
    # `bare` must fail — that's the regression we care about. The sccache row
    # may or may not also fail depending on whether sccache_seconds in the
    # bench output happens to drop too; the gate behavior under test here is
    # specifically the bare floor.
    assert "bare" in failing_baselines


def test_retry_passes_when_later_attempt_clears_threshold():
    report = perf_guard.evaluate_attempts(
        [rows(FAILING_RUST_LOG), rows(PASSING_LOG)],
        threshold=1.5,
    )

    assert report.passed
    rust_warm = [
        status
        for status in report.statuses
        if status.language == "rust"
        and status.scenario == "Build, Warm"
        and status.baseline == "bare"
    ][0]
    assert rust_warm.best_attempt == 2
    assert rust_warm.best_ratio == 90.0


def test_missing_required_language_mode_fails():
    report = perf_guard.evaluate_attempts([rows(MISSING_C_LOG)], threshold=1.5)

    assert not report.passed
    assert report.missing_requirements == [
        "c cold",
        "c warm",
        "cpp-sibling-remap / Sibling-workspace with __FILE__, Warm",
    ]


def test_missing_required_cpp_sibling_with_file_row_fails():
    log = PASSING_LOG.replace(
        "| Sibling-workspace with __FILE__, Warm | 3.000s | 1.500s | **1.000s** | **1.5x faster** | **3.0x faster** |\n",
        "",
    )

    report = perf_guard.evaluate_attempts([rows(log)])

    assert not report.passed
    assert report.missing_requirements == [
        "cpp-sibling-remap / Sibling-workspace with __FILE__, Warm"
    ]


def test_cpp_sibling_sccache_threshold_depends_on_scenario():
    log = PASSING_LOG.replace(
        "| Sibling-workspace with __FILE__, Warm | 3.000s | 1.500s | **1.000s** | **1.5x faster** | **3.0x faster** |",
        "| Sibling-workspace with __FILE__, Warm | 3.000s | 1.400s | **1.000s** | **1.4x faster** | **3.0x faster** |",
    )

    report = perf_guard.evaluate_attempts([rows(log)])

    assert not report.passed
    sibling_sccache = [
        status
        for status in report.statuses
        if status.key.benchmark == "cpp-sibling-remap" and status.baseline == "sccache"
    ]
    assert {
        (status.scenario, status.best_ratio, status.threshold, status.passed)
        for status in sibling_sccache
    } == {
        ("Sibling-workspace no __FILE__, Warm", 1.0, 1.0, True),
        ("Sibling-workspace with __FILE__, Warm", 1.4, 1.5, False),
    }


def test_language_filter_requires_only_selected_language():
    report = perf_guard.evaluate_attempts(
        [rows(MISSING_C_LOG)],
        threshold=1.5,
        languages=("rust",),
    )

    assert report.passed
    assert not report.missing_requirements
    assert {status.language for status in report.statuses} == {"rust"}


def test_all_command_failures_fail_even_when_rows_parse():
    report = perf_guard.evaluate_attempts(
        [rows(PASSING_LOG), rows(PASSING_LOG)],
        threshold=1.5,
        command_failures=[1, 2],
    )

    assert not report.passed


def test_distribution_mode_requires_every_attempt_to_succeed():
    attempts = [rows(PASSING_LOG), rows(PASSING_LOG)]

    retry_report = perf_guard.evaluate_attempts(
        attempts,
        threshold=1.5,
        command_failures=[1],
    )
    distribution_report = perf_guard.evaluate_attempts(
        attempts,
        threshold=1.5,
        command_failures=[1],
        require_all_attempts=True,
    )

    assert retry_report.passed
    assert not distribution_report.passed


def test_distribution_mode_requires_complete_numeric_samples():
    first = rows(PASSING_LOG)
    second = rows(PASSING_LOG)
    second[0]["zccache_vs_sccache_ratio"] = None

    report = perf_guard.evaluate_attempts(
        [first, second],
        threshold=1.5,
        require_all_attempts=True,
    )

    assert not report.passed
    status = next(
        status
        for status in report.statuses
        if status.language == "c"
        and status.scenario == "Single-file, Cold"
        and status.baseline == "sccache"
    )
    assert status.attempts_seen == 2
    assert len(status.samples) == 1


def test_distribution_mode_requires_every_sample_to_clear_its_floor():
    attempts = [rows(FAILING_RUST_LOG), rows(PASSING_LOG)]

    retry_report = perf_guard.evaluate_attempts(attempts, threshold=1.5)
    distribution_report = perf_guard.evaluate_attempts(
        attempts,
        threshold=1.5,
        require_all_attempts=True,
    )

    assert retry_report.passed
    assert not distribution_report.passed
    assert "actual 1.200x" in perf_guard.format_final_status(distribution_report)


def test_report_marks_failed_rows():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)
    markdown = perf_guard.format_report(report, 1.5, 1.5, 1.5, 1.5)

    assert (
        "| FAIL | rust | Rust rustc | Build, Warm | Bare rustc | 1.000s | 1.400s | 1.400x | 1.50x | 1 | 1 |"
    ) in markdown
    assert "### Benchmark summary" in markdown
    assert "#### Failed checks" in markdown
    assert "#### Passed checks" in markdown


def test_report_passes_warm_row_that_display_rounded_to_zero():
    log = """
## C++ Driver-Link Benchmark: 50 .cpp objects, 5 warm trials

| Scenario | Bare clang++ | sccache | zccache | vs sccache | vs Bare clang++ |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Driver link, Warm | 0.042s | 0.046s | **0.000s** | **94x faster** | **87x faster** |
"""
    report = perf_guard.evaluate_attempts(
        [rows(log)],
        languages=("c++",),
        require_coverage=False,
    )
    markdown = perf_guard.format_report(report, 1.5, 1.5)

    assert report.passed
    assert "n/a" not in markdown
    assert (
        "| PASS | c++ | C++ driver link | Driver link, Warm | Bare clang++ | "
        "0.5ms | 42.0ms | 87.000x | 1.50x | 1 | 1 |"
    ) in markdown
    assert (
        "| PASS | c++ | C++ driver link | Driver link, Warm | sccache | "
        "0.5ms | 46.0ms | 94.000x | 1.50x | 1 | 1 |"
    ) in markdown


def test_benchmark_summary_lists_passes_and_failures():
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    summary = perf_guard.format_benchmark_summary(report)

    assert "- Passed checks: 15" in summary
    assert "- Failed checks: 1" in summary
    assert "- Missing coverage: none" in summary
    assert "- Failed benchmark attempts: none" in summary
    assert (
        "- FAIL: c C inline args / Single-file, Warm vs sccache: "
        "expected >= 1.50x, actual 1.200x (zccache 1.000s vs baseline 1.200s) "
        "(best attempt 1; seen 1)"
    ) in summary
    assert (
        "- PASS: c C inline args / Single-file, Warm vs Bare clang: "
        "expected >= 1.50x, actual 3.000x (zccache 1.000s vs baseline 3.000s) "
        "(best attempt 1; seen 1)"
    ) in summary


def test_final_status_explains_pass_with_weakest_check():
    report = perf_guard.evaluate_attempts(
        [rows(NEAR_BARE_COLD_C_LOG)],
        languages=("c",),
    )

    final_status = perf_guard.format_final_status(report)

    assert final_status == (
        "PERF GUARD OK: all checks meet configured floors; weakest check "
        "c C inline args / Single-file, Cold vs Bare clang: expected >= 0.85x, "
        "actual 0.909x (zccache 3.300s vs baseline 3.000s)."
    )


def test_final_status_explains_worst_failed_floor():
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    final_status = perf_guard.format_final_status(report)

    # baseline 1.200s at a 1.50x floor needs zccache <= 0.800s; it took
    # 1.000s, so the miss is 200ms of wall clock rather than "0.3x".
    assert final_status == (
        "PERF GUARD FAILED: 1 check below floor; worst c C inline args / "
        "Single-file, Warm vs sccache: expected >= 1.50x, actual 1.200x "
        "(zccache 1.000s vs baseline 1.200s). Missed the floor by 200.0ms."
    )


def test_final_status_explains_missing_coverage():
    report = perf_guard.evaluate_attempts([rows(MISSING_C_LOG)], threshold=1.5)

    final_status = perf_guard.format_final_status(report)

    assert final_status == (
        "PERF GUARD FAILED: missing required benchmark coverage for c cold, c warm, "
        "cpp-sibling-remap / Sibling-workspace with __FILE__, Warm."
    )


def test_report_json_marks_thresholds_and_failed_statuses():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)
    payload = perf_guard.format_report_json(
        report,
        1.5,
        1.5,
        cold_bare_threshold=1.5,
        cold_sccache_threshold=1.5,
    )

    assert payload["schema_version"] == 2
    assert payload["passed"] is False
    assert payload["languages"] == ["c", "c++", "rust"]
    assert payload["thresholds"] == {
        "bare": 1.5,
        "sccache": 1.5,
        "cold_bare": 1.5,
        "cold_sccache": 1.5,
    }
    failing = [
        status
        for status in payload["statuses"]
        if status["language"] == "rust"
        and status["scenario"] == "Build, Warm"
        and not status["passed"]
    ]
    assert {status["baseline"] for status in failing} == {"bare", "sccache"}


def test_distribution_statistics_cover_odd_and_even_samples():
    assert perf_guard._distribution([1.0, 2.0, 9.0]) == {
        "count": 3,
        "min": 1.0,
        "median": 2.0,
        "p95": 9.0,
        "mad": 1.0,
        "max": 9.0,
    }
    assert perf_guard._distribution([1.0, 2.0, 4.0, 9.0]) == {
        "count": 4,
        "min": 1.0,
        "median": 3.0,
        "p95": 9.0,
        "mad": 1.5,
        "max": 9.0,
    }


def test_report_json_retains_attempt_distributions_and_provenance():
    second_log = PASSING_LOG.replace(
        "| Single-file, Warm | 3.000s | 2.000s | **1.000s** | **2.0x faster** | **3.0x faster** |",
        "| Single-file, Warm | 4.000s | 3.000s | **1.000s** | **3.0x faster** | **4.0x faster** |",
        1,
    )
    report = perf_guard.evaluate_attempts(
        [rows(PASSING_LOG), rows(second_log)],
        threshold=1.5,
        languages=("c",),
        require_all_attempts=True,
    )

    payload = perf_guard.format_report_json(
        report,
        1.5,
        1.5,
        ("c",),
        cold_bare_threshold=1.5,
        cold_sccache_threshold=1.5,
        metadata={"git_sha": "abc123", "dirty": False},
    )
    status = next(
        status
        for status in payload["statuses"]
        if status["scenario"] == "Single-file, Warm"
        and status["baseline"] == "bare"
    )

    assert payload["attempt_policy"] == "all-required"
    assert payload["metadata"] == {"git_sha": "abc123", "dirty": False}
    assert status["passed"] is True
    assert [sample["attempt"] for sample in status["samples"]] == [1, 2]
    assert status["samples"][1]["attempt_json"] == "attempt-2.json"
    assert status["samples"][1]["raw_log"] == "attempt-2.log"
    assert status["distributions"]["ratio"] == {
        "count": 2,
        "min": 3.0,
        "median": 3.5,
        "p95": 4.0,
        "mad": 0.5,
        "max": 4.0,
    }
    assert "## Attempt distributions" in perf_guard.format_distribution_report(
        report
    )


def test_report_json_passed_is_boolean_when_no_rows_parse():
    report = perf_guard.evaluate_attempts([[]], threshold=1.5)
    payload = perf_guard.format_report_json(report, 1.5, 1.5)

    assert payload["passed"] is False


def test_writes_perf_guard_json_artifacts(tmp_path):
    parsed_rows = rows(PASSING_LOG)
    report = perf_guard.evaluate_attempts([parsed_rows], threshold=1.5)

    perf_guard.write_attempt_json(
        tmp_path,
        1,
        parsed_rows,
        0,
        source="unit-test",
        language="c",
    )
    perf_guard.write_report_json(
        tmp_path,
        report,
        1.5,
        1.5,
        ("c",),
        cold_bare_threshold=1.5,
        cold_sccache_threshold=1.5,
    )

    attempt_payload = json.loads((tmp_path / "attempt-1.json").read_text(encoding="utf-8"))
    summary_payload = json.loads(
        (tmp_path / "perf-guard-summary.json").read_text(encoding="utf-8")
    )

    assert attempt_payload["source"] == "unit-test"
    assert attempt_payload["language"] == "c"
    assert attempt_payload["returncode"] == 0
    assert attempt_payload["row_count"] == len(parsed_rows)
    assert summary_payload["passed"] is True
    assert summary_payload["languages"] == ["c"]
    assert all(status["passed"] for status in summary_payload["statuses"])


def test_writes_perf_guard_final_status_artifact(tmp_path):
    final_status = "PERF GUARD OK: all checks meet configured floors."

    perf_guard.write_final_status(tmp_path, final_status)

    assert (tmp_path / "perf-guard-result.txt").read_text(encoding="utf-8") == (
        final_status + "\n"
    )


def test_run_attempts_stops_after_first_pass_by_default(tmp_path, monkeypatch):
    calls = 0

    def fake_run(log_path, language, benchmark_binary, test_name):
        nonlocal calls
        calls += 1
        return 0, PASSING_LOG

    monkeypatch.setattr(perf_guard, "run_benchmarks_once", fake_run)

    attempts, failures = perf_guard._run_attempts(
        tmp_path,
        3,
        1.5,
        1.5,
        1.5,
        1.5,
        perf_guard.REQUIRED_LANGUAGES,
        None,
        None,
    )

    assert calls == 1
    assert len(attempts) == 1
    assert failures == []


def test_run_attempts_collects_every_requested_sample(tmp_path, monkeypatch):
    calls = 0

    def fake_run(log_path, language, benchmark_binary, test_name):
        nonlocal calls
        calls += 1
        return 0, PASSING_LOG

    monkeypatch.setattr(perf_guard, "run_benchmarks_once", fake_run)

    attempts, failures = perf_guard._run_attempts(
        tmp_path,
        3,
        1.5,
        1.5,
        1.5,
        1.5,
        perf_guard.REQUIRED_LANGUAGES,
        None,
        None,
        collect_all_attempts=True,
    )

    assert calls == 3
    assert len(attempts) == 3
    assert failures == []
    assert sorted(path.name for path in tmp_path.glob("attempt-*.json")) == [
        "attempt-1.json",
        "attempt-2.json",
        "attempt-3.json",
    ]


def test_benchmark_language_commands_are_filtered():
    c_commands = benchmark_stats.benchmark_commands_for_language("c")
    cpp_commands = benchmark_stats.benchmark_commands_for_language("c++")
    rust_commands = benchmark_stats.benchmark_commands_for_language("rust")

    assert [command[-4] for command in c_commands] == [
        "perf_c_zccache_vs_bare",
        "perf_c_archive_link",
    ]
    assert [command[-4] for command in cpp_commands] == [
        "perf_warm_cache_zccache_vs_sccache",
        "perf_response_file",
        "perf_cpp_sibling_remap_warm",
        "perf_cpp_driver_link",
    ]
    em_commands = benchmark_stats.benchmark_commands_for_language("emscripten")
    assert [command[-4] for command in em_commands] == [
        "perf_emcc_warm_cache_zccache_vs_sccache",
        "perf_emcc_sibling_remap_warm",
        "perf_emcc_link",
    ]
    assert [command[-4] for command in rust_commands] == [
        "perf_rustc_zccache_vs_sccache",
        "perf_rustc_sibling_remap_warm",
        "perf_rust_workspace_link",
    ]


def test_unfiltered_benchmark_commands_are_reported_tests_only():
    commands = perf_guard._benchmark_commands(None)

    assert [command[command.index("--") + 1] for command in commands] == [
        test_name
        for language in benchmark_stats.LANGUAGES
        for test_name in benchmark_stats.BENCHMARK_TESTS_BY_LANGUAGE[language]
    ]


def test_prebuilt_benchmark_binary_commands_are_filtered():
    commands = perf_guard._benchmark_commands("c++", Path("perf_bench_test"))

    assert commands == [
        [
            "perf_bench_test",
            "perf_warm_cache_zccache_vs_sccache",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
        [
            "perf_bench_test",
            "perf_response_file",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
        [
            "perf_bench_test",
            "perf_cpp_sibling_remap_warm",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
        [
            "perf_bench_test",
            "perf_cpp_driver_link",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
    ]


def test_production_rust_perf_command_uses_prebuilt_test_filter():
    commands = perf_guard._benchmark_commands(
        "rust",
        Path("perf-guard-bin/perf_bench_test"),
        "perf_rust_workspace_link",
    )

    assert commands == [
        [
            str(Path("perf-guard-bin/perf_bench_test")),
            "perf_rust_workspace_link",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ]
    ]


def test_perf_workflow_has_dedicated_cow_materialization_gate():
    workflow = (perf_guard.REPO_ROOT / ".github/workflows/perf-guard.yml").read_text(
        encoding="utf-8"
    )

    assert "\n  pull_request:\n" in workflow
    job = workflow.split("\n  cow-materialization-budget:\n", 1)[1].split(
        "\n  build-perf-benchmark:\n", 1
    )[0]
    build_job = workflow.split("\n  build-perf-benchmark:\n", 1)[1].split(
        "\n  perf-guard:\n", 1
    )[0]
    speed_floor_job = workflow.split("\n  perf-guard:\n", 1)[1]
    assert not any(line.startswith("    if:") for line in job.splitlines())
    assert "    if: github.ref == 'refs/heads/main'" in build_job
    assert "    if: github.ref == 'refs/heads/main'" in speed_floor_job
    assert "name: COW materialization hit budget" in job
    assert (
        "soldr cargo test -p zccache-daemon-core "
        "--features test-support "
        "perf_cow_materialization_128_hits_under_two_seconds" in job
    )
    assert "--no-cache" not in job + build_job + speed_floor_job
    assert (
        "--language c --test perf_c_zccache_vs_bare \\\n"
        "            --attempts 3 --cold-bare-threshold 0.80"
        in speed_floor_job
    )
    assert (
        "--language rust --test perf_rustc_zccache_vs_sccache \\\n"
        "            --attempts 3 --cold-bare-threshold 0.75"
        in speed_floor_job
    )


def test_run_benchmarks_once_uses_fresh_cache_per_command(tmp_path, monkeypatch):
    commands = [["bench", "one"], ["bench", "two"]]
    cache_dirs = [tmp_path / "cache-one", tmp_path / "cache-two"]
    made_cache_dirs: list[str] = []
    removed_cache_dirs: list[Path] = []
    command_cache_dirs: list[str] = []
    audited_cache_dirs: list[Path] = []

    monkeypatch.setattr(
        perf_guard,
        "_benchmark_commands",
        lambda language, benchmark_binary, test_name=None: commands,
    )

    def fake_mkdtemp(prefix: str) -> str:
        cache_dir = cache_dirs[len(made_cache_dirs)]
        cache_dir.mkdir()
        made_cache_dirs.append(str(cache_dir))
        return str(cache_dir)

    def fake_rmtree(path: Path, ignore_errors: bool) -> None:
        removed_cache_dirs.append(Path(path))

    def fake_streamed_command(command, **kwargs):
        command_cache_dirs.append(kwargs["env"]["ZCCACHE_CACHE_DIR"])
        kwargs["log"].write(f"{command[-1]}\n")
        kwargs["log"].flush()
        return perf_watchdog.CommandResult(0, timed_out=False)

    monkeypatch.setattr(perf_guard.tempfile, "mkdtemp", fake_mkdtemp)
    monkeypatch.setattr(perf_watchdog.shutil, "rmtree", fake_rmtree)
    monkeypatch.setattr(perf_watchdog, "run_streamed_command", fake_streamed_command)
    monkeypatch.setattr(
        perf_guard,
        "audit_cache_logs",
        lambda cache_dir, output_dir, label, context: (
            audited_cache_dirs.append(Path(cache_dir)) or True
        ),
    )

    returncode, output = perf_guard.run_benchmarks_once(tmp_path / "attempt.log", "c++")

    assert returncode == 0
    assert "one\n" in output
    assert "two\n" in output
    assert "starting one" in output
    assert "starting two" in output
    assert "finished one" in output
    assert "finished two" in output
    assert command_cache_dirs == made_cache_dirs
    assert audited_cache_dirs == cache_dirs
    assert removed_cache_dirs == cache_dirs


def test_run_benchmarks_once_stops_after_first_timeout(tmp_path, monkeypatch):
    commands = [["bench", "one"], ["bench", "two"]]
    started: list[str] = []
    monkeypatch.setattr(
        perf_guard,
        "_benchmark_commands",
        lambda language, benchmark_binary, test_name=None: commands,
    )

    def fake_streamed_command(command, **kwargs):
        started.append(command[-1])
        return perf_watchdog.CommandResult(124, timed_out=True)

    monkeypatch.setattr(perf_watchdog, "run_streamed_command", fake_streamed_command)
    monkeypatch.setattr(
        perf_guard,
        "audit_cache_logs",
        lambda cache_dir, output_dir, label, context: True,
    )
    output_dir = tmp_path / "perf-guard-output" / "synthetic"

    returncode, _ = perf_guard.run_benchmarks_once(
        output_dir / "attempt.log", "c++"
    )

    assert returncode == 124
    assert started == ["one"]
    assert (tmp_path / "perf-guard-output" / ".watchdog-timeout").is_file()


def test_run_benchmarks_audits_reported_runtime_roots_before_cleanup(
    tmp_path, monkeypatch
):
    configured = tmp_path / "configured"
    runtime = tmp_path / "zccache-test-runtime"
    configured.mkdir()
    runtime.mkdir()
    audited: list[tuple[Path, str]] = []
    removed: list[Path] = []
    monkeypatch.setattr(
        perf_guard,
        "_benchmark_commands",
        lambda language, benchmark_binary, test_name=None: [["bench", "one"]],
    )
    monkeypatch.setattr(
        perf_guard.tempfile, "mkdtemp", lambda prefix: str(configured)
    )

    def fake_streamed_command(command, **kwargs):
        sidecar = kwargs["output_dir"] / "runtime-roots" / "one.txt"
        sidecar.parent.mkdir(parents=True)
        sidecar.write_text(f"{runtime}\n", encoding="utf-8")
        return perf_watchdog.CommandResult(0, timed_out=False)

    monkeypatch.setattr(perf_watchdog, "run_streamed_command", fake_streamed_command)
    monkeypatch.setattr(
        perf_guard,
        "audit_cache_logs",
        lambda cache_dir, output_dir, label, context: (
            audited.append((Path(cache_dir), context)) or True
        ),
    )
    monkeypatch.setattr(
        perf_watchdog, "_is_owned_runtime_cache_root", lambda root: True
    )
    monkeypatch.setattr(
        perf_watchdog.shutil,
        "rmtree",
        lambda path, ignore_errors: removed.append(Path(path)),
    )

    returncode, _ = perf_guard.run_benchmarks_once(tmp_path / "attempt.log", "c++")

    assert returncode == 0
    assert audited == [
        (configured.resolve(), "integration"),
        (runtime.resolve(), "perf"),
    ]
    assert removed == [configured.resolve(), runtime.resolve()]


def test_run_benchmarks_retains_every_root_when_runtime_audit_fails(
    tmp_path, monkeypatch
):
    configured = tmp_path / "configured"
    runtime = tmp_path / "zccache-test-runtime"
    configured.mkdir()
    runtime.mkdir()
    removed: list[Path] = []
    monkeypatch.setattr(
        perf_guard,
        "_benchmark_commands",
        lambda language, benchmark_binary, test_name=None: [["bench", "one"]],
    )
    monkeypatch.setattr(
        perf_guard.tempfile, "mkdtemp", lambda prefix: str(configured)
    )

    def fake_streamed_command(command, **kwargs):
        sidecar = kwargs["output_dir"] / "runtime-roots" / "one.txt"
        sidecar.parent.mkdir(parents=True)
        sidecar.write_text(f"{runtime}\n", encoding="utf-8")
        return perf_watchdog.CommandResult(0, timed_out=False)

    monkeypatch.setattr(perf_watchdog, "run_streamed_command", fake_streamed_command)
    monkeypatch.setattr(
        perf_guard,
        "audit_cache_logs",
        lambda cache_dir, output_dir, label, context: (
            Path(cache_dir) != runtime.resolve()
        ),
    )
    monkeypatch.setattr(
        perf_watchdog.shutil,
        "rmtree",
        lambda path, ignore_errors: removed.append(Path(path)),
    )

    returncode, output = perf_guard.run_benchmarks_once(
        tmp_path / "attempt.log", "c++"
    )

    assert returncode == 1
    assert removed == []
    assert "retaining cache roots after log audit failure" in output


def test_benchmark_env_uses_auto_priority_and_profiles_rust(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("RUSTC_WRAPPER", "sccache")

    env = perf_guard._benchmark_env(tmp_path, "rust")

    assert env["ZCCACHE_CACHE_DIR"] == str(tmp_path)
    assert env["ZCCACHE_COMPILE_PRIORITY"] == "auto"
    assert env["ZCCACHE_PROFILE_RUST_MISS"] == "1"
    assert "RUSTC_WRAPPER" not in env


def test_benchmark_env_does_not_enable_rust_profile_for_other_languages(tmp_path):
    env = perf_guard._benchmark_env(tmp_path, "c++")

    assert env["ZCCACHE_COMPILE_PRIORITY"] == "auto"
    assert "ZCCACHE_PROFILE_RUST_MISS" not in env


def test_benchmark_env_enables_cc_profile_for_cc_languages(tmp_path):
    # Issue #535: the C/C++ / emscripten language paths must opt into the
    # non-rustc cold-miss profile so perf-guard logs include phase data
    # for c-static-library-link / cpp-driver-link / emscripten-link cold
    # rows — the prerequisite for extending #533's overlap pattern.
    for lang in ("c", "c++", "emscripten"):
        env = perf_guard._benchmark_env(tmp_path, lang)
        assert env.get("ZCCACHE_PROFILE_CC_MISS") == "1", lang
        # And the rust profile must NOT be set for these languages.
        assert "ZCCACHE_PROFILE_RUST_MISS" not in env, lang


def test_benchmark_env_does_not_enable_cc_profile_for_rust(tmp_path):
    env = perf_guard._benchmark_env(tmp_path, "rust")
    assert "ZCCACHE_PROFILE_CC_MISS" not in env


# ── attempt spread in failure output (#1445) ───────────────────────────────
#
# A lone "actual 0.849x" cannot distinguish a regression from run-to-run
# variance. The distribution is already retained (#1115); these pin that it
# reaches the line a reader actually sees.

SLOWER_SCCACHE_LOG = PASSING_LOG.replace(
    "| Single-file, Warm | 3.000s | 2.000s | **1.000s** | **2.0x faster** | **3.0x faster** |",
    "| Single-file, Warm | 3.000s | 1.100s | **1.000s** | **1.1x faster** | **3.0x faster** |",
)


def test_failure_line_reports_every_attempt_ratio():
    report = perf_guard.evaluate_attempts(
        [rows(FAILING_SCCACHE_LOG), rows(SLOWER_SCCACHE_LOG)],
        threshold=1.5,
    )

    final_status = perf_guard.format_final_status(report)

    assert "Attempt ratios: 1.200x, 1.100x" in final_status, final_status


def test_summary_line_reports_every_attempt_ratio():
    report = perf_guard.evaluate_attempts(
        [rows(FAILING_SCCACHE_LOG), rows(SLOWER_SCCACHE_LOG)],
        threshold=1.5,
    )
    failed = [s for s in report.statuses if not report.status_passed(s)]

    line = perf_guard._format_status_summary_line(report, failed[0])

    assert "ratios 1.200x, 1.100x" in line, line


def test_single_attempt_reports_no_spread():
    """One sample has no spread to report, so the line stays as it was --
    which is why the pre-existing message assertions still hold."""
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    final_status = perf_guard.format_final_status(report)

    assert "Attempt ratios" not in final_status
    assert final_status.endswith(
        "(zccache 1.000s vs baseline 1.200s). Missed the floor by 200.0ms."
    )


def test_attempt_spread_is_ordered_by_attempt_not_by_ratio():
    """Attempt order is the informative order: it shows drift across a run.
    Sorting by ratio would hide which attempt was which."""
    report = perf_guard.evaluate_attempts(
        [rows(SLOWER_SCCACHE_LOG), rows(FAILING_SCCACHE_LOG)],
        threshold=1.5,
    )
    failed = [s for s in report.statuses if not report.status_passed(s)]

    assert perf_guard.format_attempt_spread(failed[0]) == "1.100x, 1.200x"


def test_failure_line_reports_absolute_margin_to_the_floor():
    """A ratio alone hides how small the miss is in wall clock. 0.849x
    against a 0.85x floor sounds like a near miss; the milliseconds say
    whether the runner could have resolved it at all (#1445)."""
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    assert "Missed the floor by 200.0ms." in perf_guard.format_final_status(report)


def test_floor_margin_is_empty_for_a_passing_sample():
    """No margin to report when the sample already clears its floor."""
    report = perf_guard.evaluate_attempts([rows(PASSING_LOG)], threshold=1.5)
    status = report.statuses[0]

    margin = perf_guard.format_floor_margin(
        status, perf_guard._selected_sample(report, status)
    )

    assert margin == ""
