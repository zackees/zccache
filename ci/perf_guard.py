"""Fail CI when zccache performance drops below compiler-cache speed floors."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ci import benchmark_stats


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "perf-guard-output"
DEFAULT_WARM_BARE_THRESHOLD = 1.5
DEFAULT_WARM_SCCACHE_THRESHOLD = 1.5
DEFAULT_COLD_BARE_THRESHOLD = 0.85
DEFAULT_COLD_SCCACHE_THRESHOLD = 1.0
DEFAULT_BARE_THRESHOLD = DEFAULT_WARM_BARE_THRESHOLD
DEFAULT_SCCACHE_THRESHOLD = DEFAULT_WARM_SCCACHE_THRESHOLD
DEFAULT_THRESHOLD = DEFAULT_WARM_BARE_THRESHOLD
DEFAULT_ATTEMPTS = 3
REQUIRED_LANGUAGES = ("c", "c++", "rust")
OPTIONAL_LANGUAGES = ("emscripten",)
KNOWN_LANGUAGES = REQUIRED_LANGUAGES + OPTIONAL_LANGUAGES
REQUIRED_MODES = ("cold", "warm")
CPP_SIBLING_REMAP_BENCHMARK = "cpp-sibling-remap"
CPP_SIBLING_REMAP_NO_FILE_SCENARIO = "Sibling-workspace no __FILE__, Warm"
CPP_SIBLING_REMAP_WITH_FILE_SCENARIO = "Sibling-workspace with __FILE__, Warm"
CPP_SIBLING_REMAP_NO_FILE_SCCACHE_THRESHOLD = 1.0
CPP_COLD_SCCACHE_THRESHOLD = 0.9
C_STATIC_LIBRARY_LINK_BENCHMARK = "c-static-library-link"
C_STATIC_LIBRARY_LINK_COLD_SCENARIO = "Static archive, Cold"
C_STATIC_LIBRARY_LINK_COLD_BARE_THRESHOLD = 0.75
C_STATIC_LIBRARY_LINK_COLD_SCCACHE_THRESHOLD = 0.75
RUST_BUILD_COLD_SCENARIO = "Build, Cold"
RUST_BUILD_COLD_BARE_THRESHOLD = 0.4
RUST_BUILD_COLD_SCCACHE_THRESHOLD = 0.6
# Issue #517: rust-workspace-link Cold ran at 0.283x bare on the 2026-05-31
# baseline (127 ms zccache vs 36 ms bare — 91 ms of daemon-side overhead on
# top of a 36 ms link). Documented in `benchmark-stats/latest.json` and
# needs an explicit floor so any further regression fails CI instead of
# silently sliding. Threshold leaves ~30% headroom under the baseline;
# tighten once the cold path is profiled and the dominant phase is fixed.
RUST_WORKSPACE_LINK_BENCHMARK = "rust-workspace-link"
RUST_WORKSPACE_LINK_COLD_SCENARIO = "Workspace staticlib link, Cold"
RUST_WORKSPACE_LINK_COLD_BARE_THRESHOLD = 0.2
RUST_WORKSPACE_LINK_COLD_SCCACHE_THRESHOLD = 0.85


@dataclass(frozen=True)
class ScenarioKey:
    benchmark: str
    scenario: str
    baseline: str


@dataclass
class ScenarioStatus:
    key: ScenarioKey
    benchmark_label: str
    language: str
    mode: str
    scenario: str
    baseline: str
    baseline_label: str
    threshold: float
    best_ratio: float | None = None
    best_attempt: int | None = None
    attempts_seen: int = 0
    best_zccache_seconds: float | None = None
    best_baseline_seconds: float | None = None

    @property
    def passed(self) -> bool:
        return self.best_ratio is not None and self.best_ratio >= self.threshold


@dataclass
class GuardReport:
    statuses: list[ScenarioStatus]
    missing_requirements: list[str]
    command_failures: list[int]
    attempt_count: int

    @property
    def passed(self) -> bool:
        return (
            not self.missing_requirements
            and len(self.command_failures) < self.attempt_count
            and bool(self.statuses)
            and all(status.passed for status in self.statuses)
        )


def _benchmark_binary_command(benchmark_binary: Path, test_name: str | None = None) -> list[str]:
    command = [str(benchmark_binary)]
    if test_name is not None:
        command.append(test_name)
    command.extend(["--nocapture", "--ignored", "--test-threads=1"])
    return command


def _benchmark_commands(
    language: str | None,
    benchmark_binary: Path | None = None,
    test_name: str | None = None,
) -> list[list[str]]:
    if benchmark_binary is not None:
        if test_name is not None:
            return [_benchmark_binary_command(benchmark_binary, test_name)]
        if language is None:
            return [_benchmark_binary_command(benchmark_binary)]
        return [
            _benchmark_binary_command(benchmark_binary, name)
            for name in benchmark_stats.BENCHMARK_TESTS_BY_LANGUAGE[language]
        ]
    if test_name is not None:
        return [benchmark_stats.benchmark_command_for_test(test_name)]
    if language is None:
        return [benchmark_stats.BENCHMARK_COMMAND]
    return benchmark_stats.benchmark_commands_for_language(language)


def _format_elapsed(seconds: float) -> str:
    total = int(seconds)
    minutes, secs = divmod(total, 60)
    return f"{minutes:d}m{secs:02d}s"


def _command_label(command: list[str]) -> str:
    if len(command) >= 2 and not command[1].startswith("--"):
        return command[1]
    return Path(command[0]).name


def run_benchmarks_once(
    log_path: Path,
    language: str | None = None,
    benchmark_binary: Path | None = None,
    test_name: str | None = None,
) -> tuple[int, str]:
    outputs: list[str] = []
    returncode = 0
    commands = _benchmark_commands(language, benchmark_binary, test_name)
    total_start = time.monotonic()
    for cmd_index, command in enumerate(commands, start=1):
        label = _command_label(command)
        banner = (
            f"[perf-guard] [{cmd_index}/{len(commands)}] "
            f"[T+{_format_elapsed(time.monotonic() - total_start)}] "
            f"starting {label}\n"
        )
        sys.stdout.write(banner)
        sys.stdout.flush()
        outputs.append(banner)

        cache_dir = Path(tempfile.mkdtemp(prefix="zccache-perf-guard-cache-"))
        env = _benchmark_env(cache_dir, language)
        cmd_start = time.monotonic()
        try:
            with subprocess.Popen(
                command,
                cwd=REPO_ROOT,
                env=env,
                text=True,
                encoding="utf-8",
                errors="replace",
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                bufsize=1,
            ) as proc:
                assert proc.stdout is not None
                for line in proc.stdout:
                    sys.stdout.write(line)
                    sys.stdout.flush()
                    outputs.append(line)
                proc.wait()
                cmd_returncode = proc.returncode
            if cmd_returncode != 0 and returncode == 0:
                returncode = cmd_returncode
            cmd_elapsed = time.monotonic() - cmd_start
            footer = (
                f"[perf-guard] [{cmd_index}/{len(commands)}] "
                f"finished {label} "
                f"(exit={cmd_returncode}, elapsed={_format_elapsed(cmd_elapsed)})\n"
            )
            sys.stdout.write(footer)
            sys.stdout.flush()
            outputs.append(footer)
        finally:
            shutil.rmtree(cache_dir, ignore_errors=True)

    output = "".join(outputs)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(output, encoding="utf-8")
    return returncode, output


def _benchmark_env(cache_dir: Path, language: str | None) -> dict[str, str]:
    env = os.environ.copy()
    env["ZCCACHE_CACHE_DIR"] = str(cache_dir)
    env["ZCCACHE_COMPILE_PRIORITY"] = "auto"
    if language == "rust":
        env["ZCCACHE_PROFILE_RUST_MISS"] = "1"
    elif language in ("c", "c++", "emscripten"):
        # Issue #535: emit the non-rustc cold-miss profile for
        # c-static-library-link / cpp-driver-link / emscripten-link
        # cold rows so perf-guard logs include phase data the same way
        # the rust language path already does.
        env["ZCCACHE_PROFILE_CC_MISS"] = "1"
    env.pop("RUSTC_WRAPPER", None)
    return env


def _required_rows(
    rows: list[dict[str, Any]],
    languages: tuple[str, ...],
) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row.get("language") in languages and row.get("mode") in REQUIRED_MODES
    ]


def _required_scenarios(languages: tuple[str, ...]) -> tuple[tuple[str, str], ...]:
    if "c++" not in languages:
        return ()
    return ((CPP_SIBLING_REMAP_BENCHMARK, CPP_SIBLING_REMAP_WITH_FILE_SCENARIO),)


def _comparison_threshold(
    row: dict[str, Any],
    baseline: str,
    bare_floor: float,
    sccache_floor: float,
) -> float:
    if (
        row.get("language") == "rust"
        and row.get("benchmark") == "rust"
        and row.get("scenario") == RUST_BUILD_COLD_SCENARIO
        and row.get("mode") == "cold"
    ):
        if baseline == "bare":
            return RUST_BUILD_COLD_BARE_THRESHOLD
        return RUST_BUILD_COLD_SCCACHE_THRESHOLD
    if (
        row.get("language") == "rust"
        and row.get("benchmark") == RUST_WORKSPACE_LINK_BENCHMARK
        and row.get("scenario") == RUST_WORKSPACE_LINK_COLD_SCENARIO
        and row.get("mode") == "cold"
    ):
        if baseline == "bare":
            return RUST_WORKSPACE_LINK_COLD_BARE_THRESHOLD
        return RUST_WORKSPACE_LINK_COLD_SCCACHE_THRESHOLD
    if (
        row.get("language") == "c"
        and row.get("benchmark") == C_STATIC_LIBRARY_LINK_BENCHMARK
        and row.get("scenario") == C_STATIC_LIBRARY_LINK_COLD_SCENARIO
        and row.get("mode") == "cold"
    ):
        if baseline == "bare":
            return C_STATIC_LIBRARY_LINK_COLD_BARE_THRESHOLD
        return C_STATIC_LIBRARY_LINK_COLD_SCCACHE_THRESHOLD
    if baseline == "bare":
        return bare_floor
    if (
        row.get("language") == "c++"
        and row.get("benchmark") == CPP_SIBLING_REMAP_BENCHMARK
        and row.get("scenario") == CPP_SIBLING_REMAP_NO_FILE_SCENARIO
        and row.get("mode") == "warm"
    ):
        return CPP_SIBLING_REMAP_NO_FILE_SCCACHE_THRESHOLD
    if row.get("language") == "c++" and row.get("mode") == "cold":
        return CPP_COLD_SCCACHE_THRESHOLD
    return sccache_floor


def evaluate_attempts(
    attempts: list[list[dict[str, Any]]],
    *,
    threshold: float | None = None,
    bare_threshold: float = DEFAULT_BARE_THRESHOLD,
    sccache_threshold: float = DEFAULT_SCCACHE_THRESHOLD,
    cold_bare_threshold: float = DEFAULT_COLD_BARE_THRESHOLD,
    cold_sccache_threshold: float = DEFAULT_COLD_SCCACHE_THRESHOLD,
    command_failures: list[int] | None = None,
    languages: tuple[str, ...] = REQUIRED_LANGUAGES,
    require_coverage: bool = True,
) -> GuardReport:
    if threshold is not None:
        bare_threshold = threshold
        sccache_threshold = threshold
        cold_bare_threshold = threshold
        cold_sccache_threshold = threshold
    statuses: dict[ScenarioKey, ScenarioStatus] = {}
    language_modes: set[tuple[str, str]] = set()
    seen_scenarios: set[tuple[str, str]] = set()

    for attempt_index, rows in enumerate(attempts, start=1):
        for row in _required_rows(rows, languages):
            language = str(row["language"])
            mode = str(row["mode"])
            language_modes.add((language, mode))
            seen_scenarios.add((str(row["benchmark"]), str(row["scenario"])))
            bare_floor = cold_bare_threshold if mode == "cold" else bare_threshold
            sccache_floor = cold_sccache_threshold if mode == "cold" else sccache_threshold
            comparisons = (
                (
                    "bare",
                    str(row["bare_label"]),
                    row.get("zccache_vs_bare_ratio"),
                    row.get("bare_seconds"),
                ),
                (
                    "sccache",
                    "sccache",
                    row.get("zccache_vs_sccache_ratio"),
                    row.get("sccache_seconds"),
                ),
            )
            for baseline, baseline_label, ratio, baseline_seconds in comparisons:
                ratio_threshold = _comparison_threshold(
                    row,
                    baseline,
                    bare_floor,
                    sccache_floor,
                )
                key = ScenarioKey(str(row["benchmark"]), str(row["scenario"]), baseline)
                status = statuses.get(key)
                if status is None:
                    status = ScenarioStatus(
                        key=key,
                        benchmark_label=str(row["benchmark_label"]),
                        language=language,
                        mode=mode,
                        scenario=str(row["scenario"]),
                        baseline=baseline,
                        baseline_label=baseline_label,
                        threshold=ratio_threshold,
                    )
                    statuses[key] = status
                status.attempts_seen += 1

                if isinstance(ratio, int | float):
                    if status.best_ratio is None or ratio > status.best_ratio:
                        status.best_ratio = float(ratio)
                        status.best_attempt = attempt_index
                        if isinstance(row.get("zccache_seconds"), int | float):
                            status.best_zccache_seconds = float(row["zccache_seconds"])
                        if isinstance(baseline_seconds, int | float):
                            status.best_baseline_seconds = float(baseline_seconds)

    if require_coverage:
        missing = [
            f"{language} {mode}"
            for language in languages
            for mode in REQUIRED_MODES
            if (language, mode) not in language_modes
        ]
        missing.extend(
            f"{benchmark} / {scenario}"
            for benchmark, scenario in _required_scenarios(languages)
            if (benchmark, scenario) not in seen_scenarios
        )
    else:
        missing = []

    ordered = sorted(
        statuses.values(),
        key=lambda item: (
            item.language,
            item.benchmark_label,
            item.mode,
            item.scenario,
            item.baseline,
        ),
    )
    return GuardReport(
        statuses=ordered,
        missing_requirements=missing,
        command_failures=command_failures or [],
        attempt_count=len(attempts),
    )


def _format_seconds(value: float | None) -> str:
    if value is None:
        return "n/a"
    if value >= 1.0:
        return f"{value:.3f}s"
    return f"{value * 1000:.1f}ms"


def format_report(
    report: GuardReport,
    bare_threshold: float,
    sccache_threshold: float,
    cold_bare_threshold: float = DEFAULT_COLD_BARE_THRESHOLD,
    cold_sccache_threshold: float = DEFAULT_COLD_SCCACHE_THRESHOLD,
) -> str:
    lines = [
        "## zccache perf guard",
        "",
        f"Warm bare threshold: bare compiler / zccache >= {bare_threshold:.2f}x",
        f"Warm sccache threshold: pinned sccache / zccache >= {sccache_threshold:.2f}x",
        f"Cold bare threshold: bare compiler / zccache >= {cold_bare_threshold:.2f}x",
        f"Cold sccache threshold: pinned sccache / zccache >= {cold_sccache_threshold:.2f}x",
        "",
        "| Status | Language | Benchmark | Scenario | Baseline | zccache | baseline | Ratio | Threshold | Attempt | Seen |",
        "|---|---|---|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for status in report.statuses:
        state = "PASS" if status.passed else "FAIL"
        ratio = "n/a" if status.best_ratio is None else f"{status.best_ratio:.3f}x"
        zc_time = _format_seconds(status.best_zccache_seconds)
        bl_time = _format_seconds(status.best_baseline_seconds)
        attempt = "n/a" if status.best_attempt is None else str(status.best_attempt)
        lines.append(
            "| "
            f"{state} | {status.language} | {status.benchmark_label} | "
            f"{status.scenario} | {status.baseline_label} | {zc_time} | {bl_time} | {ratio} | "
            f"{status.threshold:.2f}x | {attempt} | {status.attempts_seen} |"
        )

    if report.missing_requirements:
        lines.extend(["", "### Missing required coverage", ""])
        lines.extend(f"- {item}" for item in report.missing_requirements)

    if report.command_failures:
        lines.extend(["", "### Failed benchmark attempts", ""])
        lines.extend(f"- Attempt {attempt}" for attempt in report.command_failures)

    lines.extend(["", format_benchmark_summary(report).rstrip()])

    return "\n".join(lines) + "\n"


def _format_status_check(status: ScenarioStatus) -> str:
    actual = "n/a" if status.best_ratio is None else f"{status.best_ratio:.3f}x"
    zc_time = _format_seconds(status.best_zccache_seconds)
    bl_time = _format_seconds(status.best_baseline_seconds)
    return (
        f"{status.language} {status.benchmark_label} / {status.scenario} "
        f"vs {status.baseline_label}: expected >= {status.threshold:.2f}x, "
        f"actual {actual} (zccache {zc_time} vs baseline {bl_time})"
    )


def _format_status_summary_line(status: ScenarioStatus) -> str:
    attempt = "n/a" if status.best_attempt is None else str(status.best_attempt)
    state = "PASS" if status.passed else "FAIL"
    return (
        f"- {state}: {_format_status_check(status)} "
        f"(best attempt {attempt}; seen {status.attempts_seen})"
    )


def format_benchmark_summary(report: GuardReport) -> str:
    passed = [status for status in report.statuses if status.passed]
    failed = [status for status in report.statuses if not status.passed]
    lines = [
        "### Benchmark summary",
        "",
        f"- Passed checks: {len(passed)}",
        f"- Failed checks: {len(failed)}",
    ]

    if report.missing_requirements:
        lines.append(f"- Missing coverage: {', '.join(report.missing_requirements)}")
    else:
        lines.append("- Missing coverage: none")

    if report.command_failures:
        attempts = ", ".join(str(attempt) for attempt in report.command_failures)
        lines.append(f"- Failed benchmark attempts: {attempts}")
    else:
        lines.append("- Failed benchmark attempts: none")

    if failed:
        lines.extend(["", "#### Failed checks", ""])
        lines.extend(_format_status_summary_line(status) for status in failed)

    if passed:
        lines.extend(["", "#### Passed checks", ""])
        lines.extend(_format_status_summary_line(status) for status in passed)

    return "\n".join(lines) + "\n"


def format_final_status(report: GuardReport) -> str:
    if report.passed:
        weakest = min(
            report.statuses,
            key=lambda status: (
                float("inf")
                if status.best_ratio is None
                else status.best_ratio / status.threshold
            ),
        )
        return (
            "PERF GUARD OK: all checks meet configured floors; weakest check "
            f"{_format_status_check(weakest)}."
        )

    failed_statuses = [status for status in report.statuses if not status.passed]
    if failed_statuses:
        worst = min(
            failed_statuses,
            key=lambda status: (
                -1.0 if status.best_ratio is None else status.best_ratio / status.threshold
            ),
        )
        count = len(failed_statuses)
        return (
            f"PERF GUARD FAILED: {count} check{'s' if count != 1 else ''} "
            f"below floor; worst {_format_status_check(worst)}."
        )

    if report.missing_requirements:
        missing = ", ".join(report.missing_requirements)
        return f"PERF GUARD FAILED: missing required benchmark coverage for {missing}."

    if report.command_failures:
        attempts = ", ".join(str(attempt) for attempt in report.command_failures)
        return (
            "PERF GUARD FAILED: benchmark command failed on all "
            f"{report.attempt_count} attempt{'s' if report.attempt_count != 1 else ''} "
            f"({attempts})."
        )

    return "PERF GUARD FAILED: no benchmark rows were parsed."


def format_report_json(
    report: GuardReport,
    bare_threshold: float,
    sccache_threshold: float,
    languages: tuple[str, ...] = REQUIRED_LANGUAGES,
    *,
    cold_bare_threshold: float = DEFAULT_COLD_BARE_THRESHOLD,
    cold_sccache_threshold: float = DEFAULT_COLD_SCCACHE_THRESHOLD,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "passed": report.passed,
        "languages": list(languages),
        "thresholds": {
            "bare": bare_threshold,
            "sccache": sccache_threshold,
            "cold_bare": cold_bare_threshold,
            "cold_sccache": cold_sccache_threshold,
        },
        "attempt_count": report.attempt_count,
        "command_failures": report.command_failures,
        "missing_requirements": report.missing_requirements,
        "statuses": [
            {
                "passed": status.passed,
                "benchmark": status.key.benchmark,
                "benchmark_label": status.benchmark_label,
                "language": status.language,
                "mode": status.mode,
                "scenario": status.scenario,
                "baseline": status.baseline,
                "baseline_label": status.baseline_label,
                "threshold": status.threshold,
                "best_ratio": status.best_ratio,
                "best_attempt": status.best_attempt,
                "attempts_seen": status.attempts_seen,
            }
            for status in report.statuses
        ],
    }


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_attempt_json(
    output_dir: Path,
    attempt: int,
    rows: list[dict[str, Any]],
    returncode: int | None,
    *,
    source: str,
    language: str | None = None,
) -> None:
    _write_json(
        output_dir / f"attempt-{attempt}.json",
        {
            "schema_version": 1,
            "attempt": attempt,
            "source": source,
            "language": language,
            "returncode": returncode,
            "row_count": len(rows),
            "rows": rows,
        },
    )


def write_report_json(
    output_dir: Path,
    report: GuardReport,
    bare_threshold: float,
    sccache_threshold: float,
    languages: tuple[str, ...] = REQUIRED_LANGUAGES,
    *,
    cold_bare_threshold: float = DEFAULT_COLD_BARE_THRESHOLD,
    cold_sccache_threshold: float = DEFAULT_COLD_SCCACHE_THRESHOLD,
) -> None:
    _write_json(
        output_dir / "perf-guard-summary.json",
        format_report_json(
            report,
            bare_threshold,
            sccache_threshold,
            languages,
            cold_bare_threshold=cold_bare_threshold,
            cold_sccache_threshold=cold_sccache_threshold,
        ),
    )


def write_final_status(output_dir: Path, final_status: str) -> None:
    path = output_dir / "perf-guard-result.txt"
    path.write_text(final_status + "\n", encoding="utf-8")


def _append_step_summary(markdown: str) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    with open(summary_path, "a", encoding="utf-8") as handle:
        handle.write(markdown)


def _load_input_log(path: Path) -> list[list[dict[str, Any]]]:
    text = path.read_text(encoding="utf-8")
    return [benchmark_stats.parse_benchmark_log(text)]


def _run_attempts(
    output_dir: Path,
    attempts: int,
    bare_threshold: float,
    sccache_threshold: float,
    cold_bare_threshold: float,
    cold_sccache_threshold: float,
    languages: tuple[str, ...],
    benchmark_language: str | None,
    benchmark_binary: Path | None,
    test_name: str | None = None,
    require_coverage: bool = True,
) -> tuple[list[list[dict[str, Any]]], list[int]]:
    parsed_attempts: list[list[dict[str, Any]]] = []
    command_failures: list[int] = []

    for attempt in range(1, attempts + 1):
        log_path = output_dir / f"attempt-{attempt}.log"
        print(f"=== Perf guard attempt {attempt}/{attempts} ===")
        returncode, output = run_benchmarks_once(
            log_path, benchmark_language, benchmark_binary, test_name
        )
        rows = benchmark_stats.parse_benchmark_log(output)
        parsed_attempts.append(rows)
        write_attempt_json(
            output_dir,
            attempt,
            rows,
            returncode,
            source="benchmark-binary" if benchmark_binary else "benchmark-run",
            language=benchmark_language,
        )
        if returncode != 0:
            command_failures.append(attempt)

        interim = evaluate_attempts(
            parsed_attempts,
            bare_threshold=bare_threshold,
            sccache_threshold=sccache_threshold,
            cold_bare_threshold=cold_bare_threshold,
            cold_sccache_threshold=cold_sccache_threshold,
            command_failures=command_failures,
            languages=languages,
            require_coverage=require_coverage,
        )
        if returncode == 0 and interim.passed:
            break

    return parsed_attempts, command_failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-log", type=Path, help="Evaluate a saved benchmark log.")
    parser.add_argument("--run-benchmarks", action="store_true", help="Run perf benchmarks.")
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT_DIR)
    parser.add_argument("--threshold", type=float, help="Set all thresholds to this value.")
    parser.add_argument(
        "--bare-threshold",
        type=float,
        default=DEFAULT_BARE_THRESHOLD,
        help="Warm-row bare compiler / zccache floor.",
    )
    parser.add_argument(
        "--sccache-threshold",
        type=float,
        default=DEFAULT_SCCACHE_THRESHOLD,
        help="Warm-row pinned sccache / zccache floor.",
    )
    parser.add_argument(
        "--cold-bare-threshold",
        type=float,
        default=DEFAULT_COLD_BARE_THRESHOLD,
        help="Cold-row bare compiler / zccache floor.",
    )
    parser.add_argument(
        "--cold-sccache-threshold",
        type=float,
        default=DEFAULT_COLD_SCCACHE_THRESHOLD,
        help="Cold-row pinned sccache / zccache floor.",
    )
    parser.add_argument("--attempts", type=int, default=DEFAULT_ATTEMPTS)
    parser.add_argument(
        "--benchmark-binary",
        type=Path,
        help="Run a prebuilt perf_bench_test binary instead of invoking cargo test.",
    )
    parser.add_argument(
        "--language",
        choices=KNOWN_LANGUAGES,
        help="Run and require only one benchmark language.",
    )
    parser.add_argument(
        "--test",
        help="Run a single benchmark test by name (requires --language).",
    )
    args = parser.parse_args()

    if args.threshold is not None:
        args.bare_threshold = args.threshold
        args.sccache_threshold = args.threshold
        args.cold_bare_threshold = args.threshold
        args.cold_sccache_threshold = args.threshold
    if args.bare_threshold <= 0:
        parser.error("--bare-threshold must be greater than zero")
    if args.sccache_threshold <= 0:
        parser.error("--sccache-threshold must be greater than zero")
    if args.cold_bare_threshold <= 0:
        parser.error("--cold-bare-threshold must be greater than zero")
    if args.cold_sccache_threshold <= 0:
        parser.error("--cold-sccache-threshold must be greater than zero")
    if args.attempts < 1:
        parser.error("--attempts must be at least 1")
    if bool(args.input_log) == bool(args.run_benchmarks):
        parser.error("use exactly one of --input-log or --run-benchmarks")
    if args.benchmark_binary and not args.run_benchmarks:
        parser.error("--benchmark-binary requires --run-benchmarks")
    if args.benchmark_binary and not args.benchmark_binary.is_file():
        parser.error(f"--benchmark-binary does not exist: {args.benchmark_binary}")
    if args.test and not args.language:
        parser.error("--test requires --language")
    if args.test:
        known_tests = benchmark_stats.BENCHMARK_TESTS_BY_LANGUAGE.get(args.language, ())
        if args.test not in known_tests:
            parser.error(
                f"--test {args.test!r} is not a known test for language {args.language!r}; "
                f"known tests: {', '.join(known_tests)}"
            )

    languages = (args.language,) if args.language else REQUIRED_LANGUAGES
    require_coverage = args.test is None
    args.output_dir.mkdir(parents=True, exist_ok=True)
    if args.input_log:
        attempts = _load_input_log(args.input_log)
        write_attempt_json(
            args.output_dir,
            1,
            attempts[0],
            None,
            source="input-log",
            language=args.language,
        )
        command_failures: list[int] = []
    else:
        attempts, command_failures = _run_attempts(
            args.output_dir,
            args.attempts,
            args.bare_threshold,
            args.sccache_threshold,
            args.cold_bare_threshold,
            args.cold_sccache_threshold,
            languages,
            args.language,
            args.benchmark_binary,
            test_name=args.test,
            require_coverage=require_coverage,
        )

    report = evaluate_attempts(
        attempts,
        bare_threshold=args.bare_threshold,
        sccache_threshold=args.sccache_threshold,
        cold_bare_threshold=args.cold_bare_threshold,
        cold_sccache_threshold=args.cold_sccache_threshold,
        command_failures=command_failures,
        languages=languages,
        require_coverage=require_coverage,
    )
    markdown = format_report(
        report,
        args.bare_threshold,
        args.sccache_threshold,
        args.cold_bare_threshold,
        args.cold_sccache_threshold,
    )
    final_status = format_final_status(report)
    report_path = args.output_dir / "perf-guard-summary.md"
    report_path.write_text(markdown + "\n" + final_status + "\n", encoding="utf-8")
    write_report_json(
        args.output_dir,
        report,
        args.bare_threshold,
        args.sccache_threshold,
        languages,
        cold_bare_threshold=args.cold_bare_threshold,
        cold_sccache_threshold=args.cold_sccache_threshold,
    )
    write_final_status(args.output_dir, final_status)
    print(markdown)
    print(final_status)
    _append_step_summary(markdown + "\n" + final_status + "\n")

    return 0 if report.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
