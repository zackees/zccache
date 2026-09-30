"""Nanosecond timings for the rows the guard gates on an absolute hit budget (#1807)."""

from __future__ import annotations

import json
from typing import Any

from ci import benchmark_metrics, perf_floor


def apply_metric_precision(
    rows: list[dict[str, Any]], text: str
) -> list[dict[str, Any]]:
    """Replace display-rounded timings of budget-gated warm rows with the ns record.

    The benchmark table prints durations as `{:.3}s`, i.e. whole milliseconds,
    so a 0.33 ms hit reads `0.000s` and a 0.6 ms hit reads `0.001s`; the table
    parser then reconstructs the hit from an integer-rounded ratio. That is
    fine for a ratio floor but not for a sub-millisecond absolute budget, so
    for rows in `WARM_HIT_BUDGET_SECONDS` take the nanosecond durations from
    the `ZCCACHE_BENCH_METRIC_V1` record the same run already emitted and
    recompute the ratios from them. Rows without a usable record keep their
    table values (the table can only over-report a sub-ms hit, never hide it).
    """
    records: dict[tuple[str, str], dict[str, Any]] = {}
    for line in text.splitlines():
        _, marker, encoded = line.partition(benchmark_metrics.RECORD_PREFIX)
        if not marker:
            continue
        try:
            record = json.loads(encoded)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict) and record.get("mode") == "warm":
            records[(str(record.get("benchmark")), str(record.get("scenario")))] = record
    for row in rows:
        key = (str(row.get("benchmark")), str(row.get("scenario")))
        record = records.get(key)
        if (
            record is None
            or row.get("mode") != "warm"
            or perf_floor.warm_hit_budget_seconds(*key) is None
        ):
            continue
        zccache_ns = record.get("zccache_duration_ns")
        if not isinstance(zccache_ns, int) or isinstance(zccache_ns, bool) or zccache_ns <= 0:
            continue
        row["zccache_seconds"] = zccache_ns / 1e9
        for label, ns_field, seconds_field, ratio_field in (
            ("bare", "bare_duration_ns", "bare_seconds", "zccache_vs_bare_ratio"),
            ("sccache", "sccache_duration_ns", "sccache_seconds", "zccache_vs_sccache_ratio"),
        ):
            baseline_ns = record.get(ns_field)
            if isinstance(baseline_ns, int) and not isinstance(baseline_ns, bool) and baseline_ns > 0:
                row[seconds_field] = baseline_ns / 1e9
                row[ratio_field] = round(baseline_ns / zccache_ns, 3)
    return rows
