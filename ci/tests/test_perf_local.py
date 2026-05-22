"""Tests for ci/perf_local.py — the local Docker perf harness orchestrator.

Scope: pure functions only (formatting helpers + result-summary rendering).
Docker invocation, image build, and container run are NOT tested here —
they'd require a working Docker daemon and would duplicate what the GHA
perf cluster already covers end-to-end.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path


def _load_perf_local():
    module_path = Path(__file__).resolve().parents[1] / "perf_local.py"
    spec = importlib.util.spec_from_file_location("perf_local", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


perf_local = _load_perf_local()


# ── fmt_ms ───────────────────────────────────────────────────────────────────


def test_fmt_ms_minutes_seconds():
    assert perf_local.fmt_ms(120_000) == "2m00s"


def test_fmt_ms_seconds_with_fraction():
    # >= 1000 ms shows seconds with two decimals
    assert perf_local.fmt_ms(1_500) == "1.50s"


def test_fmt_ms_sub_second_shows_ms():
    assert perf_local.fmt_ms(750) == "750ms"


def test_fmt_ms_zero_ms():
    assert perf_local.fmt_ms(0) == "0ms"


def test_fmt_ms_missing_value():
    assert perf_local.fmt_ms(None) == "—"
    assert perf_local.fmt_ms("") == "—"


def test_fmt_ms_just_under_minute():
    # 59999 ms is just under the minute boundary — formatter uses "%.2fs"
    # so the displayed value rounds to "60.00s". The minute boundary is
    # exclusive, so 59999 still goes through the seconds branch.
    assert perf_local.fmt_ms(59_999) == "60.00s"


def test_fmt_ms_exactly_one_minute():
    assert perf_local.fmt_ms(60_000) == "1m00s"


# ── fmt_bytes ────────────────────────────────────────────────────────────────


def test_fmt_bytes_gib():
    assert perf_local.fmt_bytes(2 * (1 << 30)) == "2.00 GiB"


def test_fmt_bytes_mib():
    assert perf_local.fmt_bytes(5 * (1 << 20)) == "5.0 MiB"


def test_fmt_bytes_kib():
    assert perf_local.fmt_bytes(3 * (1 << 10)) == "3.0 KiB"


def test_fmt_bytes_under_kib():
    assert perf_local.fmt_bytes(512) == "512 B"


def test_fmt_bytes_zero():
    assert perf_local.fmt_bytes(0) == "0 B"


def test_fmt_bytes_missing_value():
    assert perf_local.fmt_bytes(None) == "—"
    assert perf_local.fmt_bytes("") == "—"


# ── fmt_count_pct ────────────────────────────────────────────────────────────


def test_fmt_count_pct_with_total():
    # 95 / 118 ≈ 80.5%
    assert perf_local.fmt_count_pct(95, 118) == "95 (80.5%)"


def test_fmt_count_pct_zero_count():
    assert perf_local.fmt_count_pct(0, 100) == "0 (0.0%)"


def test_fmt_count_pct_full_hit():
    assert perf_local.fmt_count_pct(100, 100) == "100 (100.0%)"


def test_fmt_count_pct_missing_count():
    assert perf_local.fmt_count_pct(None, 100) == "—"
    assert perf_local.fmt_count_pct("", 100) == "—"


def test_fmt_count_pct_missing_total_returns_bare_count():
    # If the total is unavailable we can't compute a %, fall back to the count
    assert perf_local.fmt_count_pct(95, None) == "95"
    assert perf_local.fmt_count_pct(95, "") == "95"
    assert perf_local.fmt_count_pct(95, 0) == "95"


# ── render_summary integration ───────────────────────────────────────────────
#
# render_summary reads result.json + warm-cache-report.json from a directory
# and prints the rich table. Tests below build a fake directory structure
# and assert the function's exit code (0 = PASS, 1 = FAIL) matches the
# expected verdict for the given speedup.


def _write_scenario_results(
    tmp_path: Path,
    scenario: str,
    *,
    cold_ms: int,
    warm_ms: int,
    cache_report: dict | None,
) -> Path:
    """Build a fake `results_dir` shape matching what the runner container
    writes. Returns the directory."""
    results_dir = tmp_path / scenario
    results_dir.mkdir()

    # Per-scenario key naming, matching perf-rust-cluster.yml's evaluate step.
    if scenario == "worktree-share":
        cold_key, warm_key = "a_ms", "b_ms"
    else:
        cold_key, warm_key = "cold_ms", "warm_ms"

    result = {
        "scenario": scenario,
        cold_key: cold_ms,
        warm_key: warm_ms,
        "peak_daemon_rss_bytes": 8_000_000,
        "peak_compile_rss_bytes": 500_000_000,
    }
    (results_dir / "result.json").write_text(json.dumps(result))

    if cache_report is not None:
        report_name = "b-cache-report.json" if scenario == "worktree-share" else "warm-cache-report.json"
        (results_dir / report_name).write_text(
            json.dumps({"last_session": cache_report})
        )

    return results_dir


def test_render_summary_pass_at_threshold(tmp_path, capsys):
    """speedup >= 4.5x must return 0 (PASS)."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=90_000,
        warm_ms=20_000,  # 4.5x exactly
        cache_report={
            "compilations": 100,
            "hits": 95,
            "misses": 3,
            "non_cacheable": 2,
            "errors": 0,
            "hit_rate": 0.95,
            "bytes_written": 100 * (1 << 20),
            "bytes_read": 50 * (1 << 20),
            "time_saved_ms": 1_500,
            "unique_sources": 95,
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    assert "4.50x" in out
    assert "95 (95.0%)" in out  # hits cell shows count + ratio inline


def test_render_summary_fail_below_threshold(tmp_path, capsys):
    """speedup < 4.5x must return 1 (FAIL)."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=52_000,  # 1.15x — same numbers as the cluster pre-fix
        cache_report={
            "compilations": 146,
            "hits": 0,
            "misses": 115,
            "non_cacheable": 31,
            "errors": 3,
            "hit_rate": 0.0,
            "bytes_written": 210 * (1 << 20),
            "bytes_read": 0,
            "time_saved_ms": 0,
            "unique_sources": 115,
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 1
    out = capsys.readouterr().out
    assert "**FAIL**" in out
    assert "1.15x" in out
    # Hits cell shows 0 with the percentage; misses with theirs
    assert "0 (0.0%)" in out
    assert "115 (78.8%)" in out


def test_render_summary_handles_missing_cache_report(tmp_path, capsys):
    """When warm-cache-report.json is absent, the cache-counter columns
    fall back to em-dashes rather than crashing. The PASS/FAIL verdict
    still computes from the timing keys in result.json."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=10_000,  # 6.0x — well above threshold
        cache_report=None,
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    # Hits/Misses/Ignored cells are em-dashes when the report is missing
    assert "| — |" in out


def test_render_summary_handles_worktree_share_a_b_keys(tmp_path, capsys):
    """The worktree-share scenario uses `a_ms`/`b_ms` instead of
    cold_ms/warm_ms, and `b-cache-report.json` for the warm-side report.
    Verify the orchestrator looks up the right keys."""
    results_dir = _write_scenario_results(
        tmp_path,
        "worktree-share",
        cold_ms=18_000,
        warm_ms=3_000,  # b/a = 6x — above the 4.5x gate
        cache_report={
            "compilations": 50,
            "hits": 40,
            "misses": 8,
            "non_cacheable": 2,
            "errors": 0,
            "hit_rate": 0.83,
            "bytes_written": 0,
            "bytes_read": 0,
            "time_saved_ms": 500,
            "unique_sources": 40,
        },
    )
    rc = perf_local.render_summary(results_dir, "worktree-share", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    assert "6.00x" in out


def test_render_summary_missing_result_json_fails(tmp_path, capsys):
    """If result.json itself is missing, the run failed before emit. Return 1
    with a clear message."""
    results_dir = tmp_path / "broken"
    results_dir.mkdir()
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 1
    out = capsys.readouterr().out
    assert "result.json missing" in out


def test_render_summary_bad_timing_fails(tmp_path, capsys):
    """0-ms or negative warm_ms is a measurement bug, not a win. Return 1."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=0,
        cache_report=None,
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 1
    out = capsys.readouterr().out
    assert "bad timing" in out


# ── docker_available smoke check ─────────────────────────────────────────────
#
# `docker_available` has a 10s subprocess timeout on `docker info`, so even
# when the daemon is hung the test returns within that window.
#
# `image_exists` is intentionally NOT tested here: it shells out to
# `docker images` without a timeout (the production caller is the
# orchestrator, where the docker_available gate fires first). On a stuck
# daemon `docker images` can hang indefinitely, so directly testing it
# would block the suite. Integration coverage belongs in a separate suite
# that runs against a known-good Docker daemon with an explicit timeout
# wrapper.


def test_docker_available_returns_bool():
    result = perf_local.docker_available()
    assert isinstance(result, bool)


# ── render_phase_breakdown ────────────────────────────────────────────────────
#
# Drives directly off `SessionStats.phase_profile` (added in
# PROTOCOL_VERSION 9). Renders a phase-sorted table so a perf operator can
# spot the dominant warm-rebuild cost without leaving the harness output.


def _phase_profile_with_writeoutput_dominant() -> dict:
    """Realistic-shape PhaseProfileSummary where write_output dominates.

    Numbers are sized so the table sort order is deterministic without being
    too close to overflow-readable values. Totals are in nanoseconds (the
    wire format)."""
    return {
        "hit_count": 100,
        "miss_count": 5,
        "parse_args_ns":           5_000_000,    # 5ms
        "build_context_ns":       12_000_000,    # 12ms
        "hash_source_ns":         80_000_000,    # 80ms
        "hash_headers_ns":        76_000_000,    # 76ms — sums w/ source to 156ms
        "depgraph_check_ns":     198_000_000,    # 198ms
        "request_cache_lookup_ns": 4_000_000,    # 4ms
        "cross_root_validate_ns":  8_000_000,
        "artifact_lookup_ns":     42_000_000,    # 42ms
        "write_output_ns":       312_000_000,    # 312ms — dominant
        "bookkeeping_ns":          3_000_000,
        "total_hit_ns":          740_000_000,
        "compiler_exec_ns":      900_000_000,    # 900ms across 5 misses
        "include_scan_ns":        25_000_000,
        "hash_all_ns":            12_000_000,
        "artifact_store_ns":      18_000_000,
        "total_miss_ns":         955_000_000,
    }


def test_render_summary_includes_phase_breakdown(tmp_path, capsys):
    """When the daemon supplies phase_profile, render_summary appends a
    phase breakdown table sorted by total descending."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=10_000,  # 6x — PASS, irrelevant to this assertion
        cache_report={
            "compilations": 105,
            "hits": 100,
            "misses": 5,
            "non_cacheable": 0,
            "errors": 0,
            "bytes_written": 0,
            "time_saved_ms": 0,
            "unique_sources": 100,
            "phase_profile": _phase_profile_with_writeoutput_dominant(),
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out

    assert "Phase breakdown" in out
    assert "100 hits, 5 misses" in out
    # write_output is the dominant hit phase — 312 ms total
    assert "write_output (materialize)" in out
    assert "312.0" in out
    # Combined source+headers hash row appears with summed total (156 ms)
    assert "metadata cache (source+hdrs)" in out
    assert "156.0" in out
    # compiler_exec on the miss path (900 ms) shows up
    assert "compiler_exec" in out
    assert "900.0" in out
    # Bookkeeping (~3 ms) still renders (tiny totals are not suppressed
    # unless they are exactly zero).
    assert "bookkeeping" in out

    # Sort order: the first phase-table row after the header must be the
    # largest total (compiler_exec at 900ms beats write_output at 312ms).
    breakdown = out.split("Phase breakdown", 1)[1]
    first_data_row = next(
        line for line in breakdown.splitlines()
        if line.startswith("| ") and "Phase" not in line and "---" not in line
    )
    assert "compiler_exec" in first_data_row


def test_render_summary_handles_absent_phase_profile(tmp_path, capsys):
    """Old daemons (PROTOCOL_VERSION <= 8) don't populate phase_profile.
    Old reports also lack the field. The summary still renders cleanly
    with no phase-breakdown section."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=10_000,
        cache_report={
            "compilations": 50,
            "hits": 48,
            "misses": 2,
            "non_cacheable": 0,
            "errors": 0,
            "bytes_written": 0,
            "time_saved_ms": 0,
            "unique_sources": 48,
            # No phase_profile key — emulating an old daemon report.
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    assert "Phase breakdown" not in out


def test_render_summary_skips_phase_breakdown_when_all_counts_zero(tmp_path, capsys):
    """A daemon that started but processed no compiles will emit
    phase_profile with all zeros. Don't print a useless empty table."""
    empty_profile = {k: 0 for k in _phase_profile_with_writeoutput_dominant()}
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=10_000,
        cache_report={
            "compilations": 0,
            "hits": 0,
            "misses": 0,
            "non_cacheable": 0,
            "errors": 0,
            "bytes_written": 0,
            "time_saved_ms": 0,
            "unique_sources": 0,
            "phase_profile": empty_profile,
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "Phase breakdown" not in out
