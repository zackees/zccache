"""Versioned producer-to-ingester contract for published benchmark metrics.

The ignored Rust benchmark tests emit one JSON record per measured row. Human
Markdown tables remain in their logs, but only these validated records may
replace the public benchmark report. Legacy Markdown readers (notably
``ci.perf_guard``) remain independent of this publication contract.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass
from typing import Literal, cast, overload


RECORD_PREFIX = "ZCCACHE_BENCH_METRIC_V1 "
RECORD_VERSION = 1
Mode = Literal["cold", "warm"]
SccacheStatus = Literal["hit", "miss", "non_cacheable", "unverified", "unavailable"]

# (benchmark, scenario_id, mode) -> (human scenario, methodology)
# Distinct IDs intentionally keep per-TU cold trials separate from historical
# batched warm trials; they are not measurements of the same experiment.
EXPECTED_RECORDS: dict[tuple[str, str, str], tuple[str, str]] = {
    ("c-inline", "single-file", mode): (f"Single-file, {mode.title()}", "single-file")
    for mode in ("cold", "warm")
}
EXPECTED_RECORDS.update({
    ("c-static-library-link", "static-archive", mode):
        (f"Static archive, {mode.title()}", "archive-link")
    for mode in ("cold", "warm")
})
for benchmark, single, multi in (
    ("cpp-inline", "Single-file", "Multi-file"),
    ("cpp-response-file", "Single-file RSP", "Multi-file RSP"),
    ("emscripten", "Single-file", "Multi-file"),
):
    EXPECTED_RECORDS[(benchmark, "single-file", "cold")] = (
        f"{single}, Cold", "single-file"
    )
    EXPECTED_RECORDS[(benchmark, "single-file", "warm")] = (
        f"{single}, Warm", "single-file"
    )
    EXPECTED_RECORDS[(benchmark, "multi-file-per-tu", "cold")] = (
        f"{multi}, Cold (per-TU)", "per-translation-unit"
    )
    EXPECTED_RECORDS[(benchmark, "multi-file-batched", "warm")] = (
        f"{multi}, Warm", "batched"
    )
for benchmark, scenarios, methodology in (
    ("cpp-driver-link", ("Driver link",), "driver-link"),
    ("emscripten-link", ("HTML link", "Wasm link"), "emscripten-link"),
    ("rust-workspace-link", ("Workspace staticlib link",), "workspace-staticlib-link"),
    ("rust", ("Build", "Check"), "rustc-batch"),
):
    for scenario in scenarios:
        scenario_id = scenario.lower().replace(" ", "-")
        for mode in ("cold", "warm"):
            EXPECTED_RECORDS[(benchmark, scenario_id, mode)] = (
                f"{scenario}, {mode.title()}", methodology
            )
for benchmark, scenarios in (
    ("cpp-sibling-remap", ("Sibling-workspace no __FILE__", "Sibling-workspace with __FILE__")),
    ("emscripten-sibling-remap", ("Sibling-workspace",)),
    ("rust-sibling-remap", ("Sibling-workspace",)),
):
    for scenario in scenarios:
        scenario_id = scenario.lower().replace(" ", "-")
        EXPECTED_RECORDS[(benchmark, scenario_id, "warm")] = (
            f"{scenario}, Warm", "sibling-workspace"
        )

REQUIRED_FIELDS = {
    "schema_version", "benchmark", "language", "test_name", "scenario_id",
    "scenario", "mode", "methodology", "trial_count", "bare_label",
    "bare_duration_ns", "sccache_duration_ns", "zccache_duration_ns",
    "bare_cache_bytes", "sccache_cache_bytes", "zccache_cache_bytes",
    "sccache_evidence",
}

EXPECTED_TESTS = {
    "c-inline": "perf_c_zccache_vs_bare",
    "c-static-library-link": "perf_c_archive_link",
    "cpp-inline": "perf_warm_cache_zccache_vs_sccache",
    "cpp-response-file": "perf_response_file",
    "cpp-sibling-remap": "perf_cpp_sibling_remap_warm",
    "cpp-driver-link": "perf_cpp_driver_link",
    "emscripten": "perf_emcc_warm_cache_zccache_vs_sccache",
    "emscripten-sibling-remap": "perf_emcc_sibling_remap_warm",
    "emscripten-link": "perf_emcc_link",
    "rust": "perf_rustc_zccache_vs_sccache",
    "rust-sibling-remap": "perf_rustc_sibling_remap_warm",
    "rust-workspace-link": "perf_rust_workspace_link",
}


class MetricContractError(ValueError):
    """The producer emitted an incomplete or inconsistent benchmark run."""


@dataclass(frozen=True, slots=True)
class SccacheEvidence:
    status: SccacheStatus
    hits: int | None
    misses: int | None
    non_cacheable: int | None
    cache_location: str | None


@dataclass(frozen=True, slots=True)
class MetricRecord:
    schema_version: int
    benchmark: str
    language: str
    test_name: str
    scenario_id: str
    scenario: str
    mode: Mode
    methodology: str
    trial_count: int
    bare_label: str
    bare_duration_ns: int
    sccache_duration_ns: int | None
    zccache_duration_ns: int
    bare_cache_bytes: int
    sccache_cache_bytes: int | None
    zccache_cache_bytes: int
    sccache_evidence: SccacheEvidence


@dataclass(frozen=True, slots=True)
class BenchmarkResult:
    benchmark: str
    benchmark_label: str
    language: str
    scenario_id: str
    scenario: str
    mode: Mode
    methodology: str
    test_name: str
    trial_count: int
    bare_label: str
    bare_duration_ns: int
    sccache_duration_ns: int | None
    zccache_duration_ns: int
    bare_seconds: float
    sccache_seconds: float | None
    zccache_seconds: float
    bare_cache_bytes: int
    sccache_cache_bytes: int | None
    zccache_cache_bytes: int
    cache_bytes_reported: bool
    sccache_evidence: SccacheEvidence
    zccache_vs_sccache_ratio: float | None
    zccache_vs_bare_ratio: float
    vs_sccache_text: str
    vs_bare_text: str


@dataclass(frozen=True, slots=True)
class BenchmarkSummary:
    row_count: int
    warm_row_count: int
    cold_row_count: int
    best_warm_vs_sccache: BenchmarkResult | None
    best_warm_vs_bare: BenchmarkResult | None


@dataclass(frozen=True, slots=True)
class BenchmarkReport:
    schema_version: int
    metadata: dict[str, object]
    summary: BenchmarkSummary
    results: tuple[BenchmarkResult, ...]


def make_report(results: list[BenchmarkResult], metadata: dict[str, object]) -> BenchmarkReport:
    """Keep benchmark data typed until the output/rendering boundary."""
    warm = [row for row in results if row.mode == "warm"]
    cold = [row for row in results if row.mode == "cold"]
    best_sccache = max(
        (row for row in warm if row.zccache_vs_sccache_ratio is not None),
        key=lambda row: row.zccache_vs_sccache_ratio or 0.0,
        default=None,
    )
    best_bare = max(warm, key=lambda row: row.zccache_vs_bare_ratio, default=None)
    return BenchmarkReport(
        schema_version=3,
        metadata=metadata,
        summary=BenchmarkSummary(
            row_count=len(results),
            warm_row_count=len(warm),
            cold_row_count=len(cold),
            best_warm_vs_sccache=best_sccache,
            best_warm_vs_bare=best_bare,
        ),
        results=tuple(results),
    )


@overload
def _positive_int(value: object, field: str, *, nullable: Literal[False] = False) -> int: ...


@overload
def _positive_int(value: object, field: str, *, nullable: Literal[True]) -> int | None: ...


def _positive_int(value: object, field: str, *, nullable: bool = False) -> int | None:
    if value is None and nullable:
        return None
    if type(value) is not int or value <= 0:
        raise MetricContractError(f"{field} must be a positive integer")
    return value


@overload
def _bytes(value: object, field: str, *, nullable: Literal[False] = False) -> int: ...


@overload
def _bytes(value: object, field: str, *, nullable: Literal[True]) -> int | None: ...


def _bytes(value: object, field: str, *, nullable: bool = False) -> int | None:
    if value is None and nullable:
        return None
    if type(value) is not int or value < 0:
        raise MetricContractError(f"{field} must be a nonnegative integer")
    return value


def _require_str(value: object, field: str) -> str:
    if not isinstance(value, str):
        raise MetricContractError(f"{field} must be a string")
    return value


def _validate_evidence(evidence: object, *, mode: str, sccache_ns: int | None) -> SccacheEvidence:
    if not isinstance(evidence, dict):
        raise MetricContractError("sccache_evidence must be an object")
    evidence = cast("dict[str, object]", evidence)
    if set(evidence) != {"status", "hits", "misses", "non_cacheable", "cache_location"}:
        raise MetricContractError("sccache_evidence has missing or unknown fields")
    status = _require_str(evidence["status"], "sccache_evidence.status")
    if status not in {"hit", "miss", "non_cacheable", "unverified", "unavailable"}:
        raise MetricContractError(f"unknown sccache status {status!r}")
    for field in ("hits", "misses", "non_cacheable"):
        _bytes(evidence[field], f"sccache_evidence.{field}", nullable=True)
    location = evidence["cache_location"]
    if location is not None and (not isinstance(location, str) or not location):
        raise MetricContractError("sccache_evidence.cache_location must be a string or null")
    if (sccache_ns is None) != (status == "unavailable"):
        raise MetricContractError("sccache unavailable status and duration disagree")
    if status == "hit" and (
        mode != "warm"
        or not _bytes(evidence["hits"], "sccache_evidence.hits")
        or evidence["misses"] != 0
        or evidence["non_cacheable"] != 0
    ):
        raise MetricContractError("a verified warm hit needs hits and no misses/non-cacheable requests")
    return SccacheEvidence(
        status=cast("SccacheStatus", status),
        hits=_bytes(evidence["hits"], "sccache_evidence.hits", nullable=True),
        misses=_bytes(evidence["misses"], "sccache_evidence.misses", nullable=True),
        non_cacheable=_bytes(evidence["non_cacheable"], "sccache_evidence.non_cacheable", nullable=True),
        cache_location=location,
    )


def parse_metric_records(text: str, table_metadata: dict[str, dict[str, str]]) -> list[MetricRecord]:
    """Parse only typed producer records and require the whole declared matrix."""
    records: dict[tuple[str, str, str], MetricRecord] = {}
    for line_number, line in enumerate(text.splitlines(), 1):
        if RECORD_PREFIX not in line:
            continue
        _, _, encoded = line.partition(RECORD_PREFIX)
        try:
            raw: object = json.loads(encoded)
        except json.JSONDecodeError as exc:
            raise MetricContractError(f"line {line_number}: invalid metric JSON") from exc
        if not isinstance(raw, dict):
            raise MetricContractError(f"line {line_number}: metric must be an object")
        record = cast("dict[str, object]", raw)
        if set(record) != REQUIRED_FIELDS:
            raise MetricContractError(f"line {line_number}: metric fields do not match v1")
        if type(record["schema_version"]) is not int or record["schema_version"] != RECORD_VERSION:
            raise MetricContractError(f"line {line_number}: unsupported metric schema")
        benchmark = _require_str(record["benchmark"], "benchmark")
        language = _require_str(record["language"], "language")
        test_name = _require_str(record["test_name"], "test_name")
        scenario_id = _require_str(record["scenario_id"], "scenario_id")
        scenario = _require_str(record["scenario"], "scenario")
        mode = _require_str(record["mode"], "mode")
        if mode not in ("cold", "warm"):
            raise MetricContractError(f"line {line_number}: invalid mode")
        methodology = _require_str(record["methodology"], "methodology")
        bare_label = _require_str(record["bare_label"], "bare_label")
        key = (benchmark, scenario_id, mode)
        expected = EXPECTED_RECORDS.get(key)
        if expected is None:
            raise MetricContractError(f"line {line_number}: unexpected metric {key!r}")
        if key in records:
            raise MetricContractError(f"line {line_number}: duplicate metric {key!r}")
        if (record["scenario"], record["methodology"]) != expected:
            raise MetricContractError(f"line {line_number}: identity/methodology mismatch for {key!r}")
        table = table_metadata.get(benchmark)
        if table is None or (language, bare_label) != (
            table["language"], table["bare_label"]
        ):
            raise MetricContractError(f"line {line_number}: language/bare-label mismatch")
        if test_name != EXPECTED_TESTS[benchmark]:
            raise MetricContractError(f"line {line_number}: invalid test name")
        _positive_int(record["trial_count"], "trial_count")
        for field in ("bare_duration_ns", "zccache_duration_ns"):
            _positive_int(record[field], field)
        _positive_int(record["sccache_duration_ns"], "sccache_duration_ns", nullable=True)
        if record["bare_cache_bytes"] != 0:
            raise MetricContractError("bare compiler cache bytes must be zero")
        _bytes(record["sccache_cache_bytes"], "sccache_cache_bytes", nullable=True)
        _bytes(record["zccache_cache_bytes"], "zccache_cache_bytes")
        evidence = _validate_evidence(
            record["sccache_evidence"],
            mode=mode,
            sccache_ns=_positive_int(record["sccache_duration_ns"], "sccache_duration_ns", nullable=True),
        )
        records[key] = MetricRecord(
            schema_version=RECORD_VERSION,
            benchmark=benchmark,
            language=language,
            test_name=test_name,
            scenario_id=scenario_id,
            scenario=scenario,
            mode=mode,
            methodology=methodology,
            trial_count=_positive_int(record["trial_count"], "trial_count"),
            bare_label=bare_label,
            bare_duration_ns=_positive_int(record["bare_duration_ns"], "bare_duration_ns"),
            sccache_duration_ns=_positive_int(record["sccache_duration_ns"], "sccache_duration_ns", nullable=True),
            zccache_duration_ns=_positive_int(record["zccache_duration_ns"], "zccache_duration_ns"),
            bare_cache_bytes=_bytes(record["bare_cache_bytes"], "bare_cache_bytes"),
            sccache_cache_bytes=_bytes(record["sccache_cache_bytes"], "sccache_cache_bytes", nullable=True),
            zccache_cache_bytes=_bytes(record["zccache_cache_bytes"], "zccache_cache_bytes"),
            sccache_evidence=evidence,
        )
    missing = EXPECTED_RECORDS.keys() - records.keys()
    if missing:
        raise MetricContractError(f"missing {len(missing)} metric records: {sorted(missing)!r}")
    return [records[key] for key in EXPECTED_RECORDS]


def records_to_results(
    records: list[MetricRecord], table_metadata: dict[str, dict[str, str]]
) -> list[BenchmarkResult]:
    """Project exact-nanosecond records into the existing public row shape."""
    rows: list[BenchmarkResult] = []
    for record in records:
        bare_ns = record.bare_duration_ns
        sccache_ns = record.sccache_duration_ns
        zccache_ns = record.zccache_duration_ns
        evidence = record.sccache_evidence
        vs_sccache = (
            sccache_ns / zccache_ns
            if sccache_ns is not None and (
                (record.mode == "cold" and evidence.status in {"miss", "non_cacheable"})
                or evidence.status == "hit"
            )
            else None
        )
        vs_bare = bare_ns / zccache_ns
        if not math.isfinite(vs_bare) or (vs_sccache is not None and not math.isfinite(vs_sccache)):
            raise MetricContractError("non-finite speed ratio")
        table = table_metadata[record.benchmark]
        rows.append(BenchmarkResult(
            benchmark=record.benchmark,
            benchmark_label=table["label"],
            language=record.language,
            scenario_id=record.scenario_id,
            scenario=record.scenario,
            mode=record.mode,
            methodology=record.methodology,
            test_name=record.test_name,
            trial_count=record.trial_count,
            bare_label=record.bare_label,
            bare_duration_ns=bare_ns,
            sccache_duration_ns=sccache_ns,
            zccache_duration_ns=zccache_ns,
            bare_seconds=bare_ns / 1_000_000_000,
            sccache_seconds=sccache_ns / 1_000_000_000 if sccache_ns is not None else None,
            zccache_seconds=zccache_ns / 1_000_000_000,
            bare_cache_bytes=0,
            sccache_cache_bytes=record.sccache_cache_bytes,
            zccache_cache_bytes=record.zccache_cache_bytes,
            cache_bytes_reported=True,
            sccache_evidence=evidence,
            zccache_vs_sccache_ratio=vs_sccache,
            zccache_vs_bare_ratio=vs_bare,
            vs_sccache_text=_ratio_text(vs_sccache),
            vs_bare_text=_ratio_text(vs_bare),
        ))
    return rows


def _ratio_text(ratio: float | None) -> str:
    if ratio is None:
        return "n/a"
    if ratio >= 10:
        return f"{ratio:.0f}x faster"
    if ratio >= 1.05:
        return f"{ratio:.1f}x faster"
    if ratio > 0.95:
        return "~same"
    inverse = 1 / ratio
    return f"{inverse:.0f}x slower" if inverse >= 10 else f"{inverse:.1f}x slower"


def format_seconds(seconds: float | None) -> str:
    """Keep sub-millisecond timings legible beside exact-nanosecond ratios."""
    if seconds is None:
        return "n/a"
    if seconds >= 1:
        return f"{seconds:.3f}s"
    if seconds >= 0.000001:
        return f"{seconds * 1_000:.3f}".rstrip("0").rstrip(".") + "ms"
    return f"{seconds * 1_000_000:.1f}µs"
