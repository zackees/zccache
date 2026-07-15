"""Distribution statistics and Markdown rendering for perf guard samples."""

from __future__ import annotations

import statistics
from typing import Protocol, Sequence


class Sample(Protocol):
    ratio: float
    zccache_seconds: float
    baseline_seconds: float


class Status(Protocol):
    language: str
    benchmark_label: str
    scenario: str
    baseline_label: str
    samples: Sequence[Sample]


def distribution(values: list[float]) -> dict[str, float | int]:
    """Return nearest-rank p95 and robust summary statistics."""
    ordered = sorted(values)
    median = statistics.median(ordered)
    p95_index = max(0, (len(ordered) * 95 + 99) // 100 - 1)
    return {
        "count": len(ordered),
        "min": ordered[0],
        "median": median,
        "p95": ordered[p95_index],
        "mad": statistics.median(abs(value - median) for value in ordered),
        "max": ordered[-1],
    }


def status_distributions(
    status: Status,
) -> dict[str, dict[str, float | int]]:
    return {
        "ratio": distribution([sample.ratio for sample in status.samples]),
        "zccache_seconds": distribution(
            [sample.zccache_seconds for sample in status.samples]
        ),
        "baseline_seconds": distribution(
            [sample.baseline_seconds for sample in status.samples]
        ),
    }


def _format_seconds(value: float) -> str:
    if value >= 1.0:
        return f"{value:.3f}s"
    return f"{value * 1000:.1f}ms"


def format_markdown(statuses: Sequence[Status]) -> str:
    lines = [
        "## Attempt distributions",
        "",
        "| Language | Benchmark | Scenario | Baseline | Samples | Ratio median | Ratio MAD | zccache median | Baseline median |",
        "|---|---|---|---|---:|---:|---:|---:|---:|",
    ]
    for status in statuses:
        if not status.samples:
            lines.append(
                f"| {status.language} | {status.benchmark_label} | "
                f"{status.scenario} | {status.baseline_label} | "
                "0 | n/a | n/a | n/a | n/a |"
            )
            continue
        distributions = status_distributions(status)
        ratio = distributions["ratio"]
        zccache = distributions["zccache_seconds"]
        baseline = distributions["baseline_seconds"]
        lines.append(
            f"| {status.language} | {status.benchmark_label} | "
            f"{status.scenario} | {status.baseline_label} | {ratio['count']} | "
            f"{ratio['median']:.3f}x | {ratio['mad']:.3f}x | "
            f"{_format_seconds(float(zccache['median']))} | "
            f"{_format_seconds(float(baseline['median']))} |"
        )
    return "\n".join(lines) + "\n"
