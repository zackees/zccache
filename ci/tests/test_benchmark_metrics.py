"""Contract tests for #1754's typed Rust producer / Python publisher seam."""

import json

import pytest

from ci import benchmark_metrics, benchmark_stats


# Literal rows from the 2026-09-28 benchmark-log artifact (run 36373820598).
# They reproduce the warm-only phantom cold label, 0 B Rust sccache rows,
# and cold/warm workspace cache snapshots that motivated #1754.
SEPT_28_LOG_EXCERPT = """
## Rust Sibling-Workspace Remap Benchmark: 50 .rs files, 5 warm trials
| Scenario | Bare rustc | sccache | zccache | bare cache | sccache cache | zccache cache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|--------------:|-----------:|--------------:|
| Sibling-workspace, Warm | 1.605s | 1.713s | **0.196s** | 0 B | 0 B | 3.4 MiB | **8.7x faster** | **8.2x faster** |
## Rust Workspace Link Benchmark: 50 .rlib inputs, 5 warm trials
| Scenario | Bare rustc | sccache | zccache | bare cache | sccache cache | zccache cache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|--------------:|-----------:|--------------:|
| Workspace staticlib link, Cold | 0.035s | 0.121s | 0.064s | 0 B | 0 B | 28.5 MiB | 1.9x faster | 1.8x slower |
| Workspace staticlib link, Warm | 0.035s | 0.038s | **0.012s** | 0 B | 0 B | 23.3 MiB | **3.1x faster** | **2.9x faster** |
"""


def test_september_28_log_fixture_reproduces_warm_only_and_mode_specific_bytes():
    rows = benchmark_stats.parse_benchmark_log(SEPT_28_LOG_EXCERPT)
    assert len(rows) == 3
    combined = benchmark_stats.build_combined_image_rows(rows)
    sibling = next(row for row in combined if row["benchmark"] == "rust-sibling-remap")
    link = next(row for row in combined if row["benchmark"] == "rust-workspace-link")
    assert all(value is None for value in sibling["cold"].values())
    assert sibling["cache_bytes"]["warm"]["sccache"] == 0
    assert link["cache_bytes"]["cold"]["zccache"] != link["cache_bytes"]["warm"]["zccache"]


def typed_records():
    tables = {table["id"]: table for table in benchmark_stats.TABLES.values()}
    records = []
    for benchmark, scenario_id, mode in benchmark_metrics.EXPECTED_RECORDS:
        scenario, methodology = benchmark_metrics.EXPECTED_RECORDS[
            (benchmark, scenario_id, mode)
        ]
        table = tables[benchmark]
        records.append({
            "schema_version": 1,
            "benchmark": benchmark,
            "language": table["language"],
            "test_name": benchmark_metrics.EXPECTED_TESTS[benchmark],
            "scenario_id": scenario_id,
            "scenario": scenario,
            "mode": mode,
            "methodology": methodology,
            "trial_count": 5 if mode == "warm" else 1,
            "bare_label": table["bare_label"],
            "bare_duration_ns": 360_000_000,
            "sccache_duration_ns": 360_000_000,
            "zccache_duration_ns": 878_000,
            "bare_cache_bytes": 0,
            "sccache_cache_bytes": 1024,
            "zccache_cache_bytes": 2048,
            "sccache_evidence": {
                "status": "hit" if mode == "warm" else "miss",
                "hits": 5 if mode == "warm" else 0,
                "misses": 0 if mode == "warm" else 1,
                "non_cacheable": 0,
                "cache_location": "Local disk: fixture",
            },
        })
    return records


def metric_log(records):
    return "\n".join(
        f"0.01 {benchmark_metrics.RECORD_PREFIX}{json.dumps(record)}"
        for record in records
    )


def test_typed_contract_requires_all_32_records_and_exact_identity():
    records = typed_records()
    assert len(records) == 32
    tables = {table["id"]: table for table in benchmark_stats.TABLES.values()}
    parsed = benchmark_metrics.parse_metric_records(metric_log(records), tables)
    assert len(parsed) == 32
    assert all(isinstance(record, benchmark_metrics.MetricRecord) for record in parsed)
    for broken in (records[:-1], records + [records[0]]):
        with pytest.raises(benchmark_metrics.MetricContractError):
            benchmark_metrics.parse_metric_records(metric_log(broken), tables)

    malformed = typed_records()
    malformed[0]["sccache_cache_bytes"] = "unknown"
    with pytest.raises(benchmark_metrics.MetricContractError):
        benchmark_metrics.parse_metric_records(metric_log(malformed), tables)

    for field, value in (
        ("test_name", "perf_unrelated"),
        ("methodology", "batched"),
        ("zccache_duration_ns", "0.001s"),
        ("schema_version", 2),
    ):
        malformed = typed_records()
        malformed[0][field] = value
        with pytest.raises(benchmark_metrics.MetricContractError):
            benchmark_metrics.parse_metric_records(metric_log(malformed), tables)

    with pytest.raises(benchmark_metrics.MetricContractError):
        benchmark_metrics.parse_metric_records(
            metric_log(records[:-1]) + f"\n{benchmark_metrics.RECORD_PREFIX}{{broken",
            tables,
        )


def test_typed_contract_keeps_exact_ns_and_does_not_claim_unverified_hits():
    records = typed_records()
    tables = {table["id"]: table for table in benchmark_stats.TABLES.values()}
    warm = next(record for record in records if record["mode"] == "warm")
    warm["sccache_evidence"] = {
        "status": "unverified", "hits": None, "misses": None,
        "non_cacheable": None, "cache_location": None,
    }
    rows = benchmark_metrics.records_to_results(
        benchmark_metrics.parse_metric_records(metric_log(records), tables), tables
    )
    matching = next(
        row for row in rows
        if (row.benchmark, row.scenario_id, row.mode) ==
        (warm["benchmark"], warm["scenario_id"], "warm")
    )
    assert isinstance(matching, benchmark_metrics.BenchmarkResult)
    assert matching.zccache_duration_ns == 878_000
    assert matching.zccache_vs_bare_ratio == pytest.approx(410.023, abs=0.001)
    assert matching.vs_bare_text == "410x faster"
    assert benchmark_metrics.format_seconds(matching.zccache_seconds) == "0.878ms"
    assert benchmark_metrics.format_seconds(0.00149) == "1.49ms"
    assert matching.zccache_vs_sccache_ratio is None
    assert matching.vs_sccache_text == "n/a"


def test_publisher_consumes_typed_records_and_rejects_partial_log(tmp_path, monkeypatch):
    from PIL import ImageDraw

    captured_text = []
    original_text = ImageDraw.ImageDraw.text

    def recording_text(self, xy, value, *args, **kwargs):
        captured_text.append(value)
        return original_text(self, xy, value, *args, **kwargs)

    monkeypatch.setattr(ImageDraw.ImageDraw, "text", recording_text)
    log_path = tmp_path / "benchmark.log"
    output_dir = tmp_path / "report"
    log_path.write_text(metric_log(typed_records()), encoding="utf-8")
    assert benchmark_stats.main([
        "--input-log", str(log_path), "--output-dir", str(output_dir)
    ]) == 0
    payload = json.loads((output_dir / "latest.json").read_text(encoding="utf-8"))
    assert payload["summary"]["row_count"] == 32
    assert payload["results"][0]["bare_duration_ns"] == 360_000_000
    html = (output_dir / "index.html").read_text(encoding="utf-8")
    assert "0.878ms" in html
    assert "410.0x faster" in html
    assert "warm 0.878ms" in captured_text
    assert any("Multi-file, Cold (per-TU)" in value for value in captured_text), [
        value for value in captured_text if "Multi" in value or "per-TU" in value
    ]
    image_rows = benchmark_stats.build_combined_image_rows(payload["results"])
    per_tu = next(row for row in image_rows if row["scenario_root"] == "multi-file-per-tu")
    assert per_tu["compact_scenario"] == "Multi-file, Cold (per-TU)"
    history = json.loads((output_dir / "history.jsonl").read_text(encoding="utf-8").splitlines()[-1])
    assert history["schema_version"] == 3
    assert history["results"][0]["scenario_id"] == "single-file"
    assert history["results"][0]["zccache_duration_ns"] == 878_000

    original = (output_dir / "latest.json").read_bytes()
    log_path.write_text(metric_log(typed_records()[:-1]), encoding="utf-8")
    with pytest.raises(benchmark_metrics.MetricContractError):
        benchmark_stats.main([
            "--input-log", str(log_path), "--output-dir", str(output_dir)
        ])
    assert (output_dir / "latest.json").read_bytes() == original
