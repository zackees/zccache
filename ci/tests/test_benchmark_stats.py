import json
import re

import pytest

from ci import benchmark_stats


SAMPLE_LOG = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | — | 2.000s | — | 1.5x faster |
| Single-file, Warm | 3.000s | — | **0.050s** | — | **60x faster** |

## C Static-Library Link Benchmark: 50 .o inputs, 5 warm trials

| Scenario | Bare ar | sccache | zccache | vs sccache | vs Bare ar |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Static archive, Cold | 0.900s | 0.920s | 0.950s | 1.0x slower | 1.1x slower |
| Static archive, Warm | 0.890s | 0.910s | **0.040s** | **23x faster** | **22x faster** |

## Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 12.641s | 20.632s | 13.430s | 1.5x faster | 1.1x slower |
| Single-file, Warm | 11.705s | 1.576s | **0.050s** | **32x faster** | **236x faster** |

## Response-File Benchmark: 50 C++ files, ~283 expanded args, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file RSP, Cold | 12.063s | 20.607s | 14.087s | 1.5x faster | 1.2x slower |
| Single-file RSP, Warm | 12.540s | 1.558s | **0.047s** | **33x faster** | **267x faster** |

## Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 8.165s | 9.634s | 41.624s | 4.3x slower | 5.1x slower |
| Build, Warm | 7.018s | 8.236s | **0.123s** | **67x faster** | **57x faster** |

## C++ Sibling-Workspace Remap Benchmark: 50 .cpp files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Sibling-workspace no __FILE__, Warm | 11.812s | 1.602s | **1.602s** | **1.0x faster** | **7.4x faster** |
| Sibling-workspace with __FILE__, Warm | 11.812s | 1.602s | **0.052s** | **31x faster** | **227x faster** |

## C++ Driver-Link Benchmark: 50 .cpp objects, 5 warm trials

| Scenario | Bare clang++ | sccache | zccache | vs sccache | vs Bare clang++ |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Driver link, Cold | 2.500s | 2.600s | 2.700s | 1.0x slower | 1.1x slower |
| Driver link, Warm | 2.480s | 2.610s | **0.060s** | **44x faster** | **41x faster** |

## Rust Sibling-Workspace Remap Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Sibling-workspace, Warm | 7.142s | 8.301s | **0.127s** | **65x faster** | **56x faster** |

## Emscripten Benchmark: 50 .cpp files, 5 warm trials

| Scenario | Bare em++ | sccache | zccache | vs sccache | vs bare em++ |
|:---------|---------:|--------:|--------:|-----------:|-------------:|
| Single-file, Cold | 14.301s | 21.118s | 15.420s | 1.4x faster | 1.1x slower |
| Single-file, Warm | 13.802s | 1.703s | **0.061s** | **28x faster** | **226x faster** |

## Emscripten Sibling-Workspace Remap Benchmark: 50 .cpp files, 5 warm trials

| Scenario | Bare em++ | sccache | zccache | vs sccache | vs bare em++ |
|:---------|---------:|--------:|--------:|-----------:|-------------:|
| Sibling-workspace, Warm | 13.654s | 1.712s | **0.063s** | **27x faster** | **217x faster** |

## Emscripten Link Benchmark: 50 .cpp objects, 5 warm trials

| Scenario | Bare em++ | sccache | zccache | vs sccache | vs Bare em++ |
|:---------|---------:|--------:|--------:|-----------:|-------------:|
| HTML link, Cold | 6.500s | 6.600s | 6.800s | 1.0x slower | 1.0x slower |
| HTML link, Warm | 6.450s | 6.550s | **0.070s** | **94x faster** | **92x faster** |
| Wasm link, Cold | 4.500s | 4.600s | 4.700s | 1.0x slower | 1.0x slower |
| Wasm link, Warm | 4.450s | 4.550s | **0.065s** | **70x faster** | **68x faster** |

## Rust Workspace Link Benchmark: 50 .rlib inputs, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs Bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Workspace staticlib link, Cold | 5.500s | 5.700s | 5.900s | 1.0x slower | 1.1x slower |
| Workspace staticlib link, Warm | 5.450s | 5.650s | **0.080s** | **71x faster** | **68x faster** |
"""


def sample_payload():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)
    return benchmark_stats.build_payload(
        rows,
        {
            "generated_at": "2026-05-13T00:00:00+00:00",
            "repository": "zackees/zccache",
            "git_sha": "abcdef1234567890",
            "git_ref": "main",
            "run_url": "https://example.invalid/run",
            "runner": {"platform": "test", "os": "test", "arch": "x64", "cpu_count": 1},
            "versions": {"soldr": None, "rustc": None, "clang": None, "sccache": None},
            "benchmark_command": "soldr --no-cache cargo test ...",
            "pages_url": "https://zackees.github.io/zccache/",
            "raw_image_base_url": benchmark_stats.DEFAULT_RAW_IMAGE_BASE_URL,
            "raw_image_urls": {
                language: f"{benchmark_stats.DEFAULT_RAW_IMAGE_BASE_URL}/{image_file}"
                for language, image_file in benchmark_stats.LANGUAGE_IMAGE_FILES.items()
            },
        },
    )


def test_parse_benchmark_log_extracts_all_tables():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)

    assert len(rows) == 24
    assert {row["benchmark"] for row in rows} == {
        "c-inline",
        "c-static-library-link",
        "cpp-driver-link",
        "cpp-inline",
        "cpp-response-file",
        "cpp-sibling-remap",
        "emscripten",
        "emscripten-link",
        "emscripten-sibling-remap",
        "rust",
        "rust-workspace-link",
        "rust-sibling-remap",
    }

    c_warm = [row for row in rows if row["benchmark"] == "c-inline" and row["mode"] == "warm"][0]
    assert c_warm["language"] == "c"
    assert c_warm["zccache_vs_bare_ratio"] == 60.0

    c_link = [row for row in rows if row["benchmark"] == "c-static-library-link"]
    assert [row["mode"] for row in c_link] == ["cold", "warm"]
    assert c_link[1]["bare_label"] == "Bare ar"
    assert c_link[1]["zccache_seconds"] == 0.04

    rust_warm = [row for row in rows if row["scenario"] == "Build, Warm"][0]
    assert rust_warm["language"] == "rust"
    assert rust_warm["zccache_seconds"] == 0.123
    assert rust_warm["zccache_vs_sccache_ratio"] == 66.959

    cpp_remaps = [row for row in rows if row["benchmark"] == "cpp-sibling-remap"]
    assert [row["scenario"] for row in cpp_remaps] == [
        "Sibling-workspace no __FILE__, Warm",
        "Sibling-workspace with __FILE__, Warm",
    ]
    assert all(row["mode"] == "warm" for row in cpp_remaps)
    assert all(row["language"] == "c++" for row in cpp_remaps)
    assert [row["zccache_vs_sccache_ratio"] for row in cpp_remaps] == [1.0, 30.808]

    cpp_link = [row for row in rows if row["benchmark"] == "cpp-driver-link"]
    assert [row["scenario"] for row in cpp_link] == ["Driver link, Cold", "Driver link, Warm"]
    assert cpp_link[1]["bare_label"] == "Bare clang++"

    rust_remap = [row for row in rows if row["benchmark"] == "rust-sibling-remap"][0]
    assert rust_remap["mode"] == "warm"
    assert rust_remap["language"] == "rust"
    assert rust_remap["zccache_seconds"] == 0.127

    em_warm = [row for row in rows if row["benchmark"] == "emscripten" and row["mode"] == "warm"][
        0
    ]
    assert em_warm["language"] == "emscripten"
    assert em_warm["bare_label"] == "Bare em++"
    assert em_warm["zccache_seconds"] == 0.061

    em_remap = [row for row in rows if row["benchmark"] == "emscripten-sibling-remap"][0]
    assert em_remap["mode"] == "warm"
    assert em_remap["language"] == "emscripten"
    assert em_remap["zccache_seconds"] == 0.063

    em_link = [row for row in rows if row["benchmark"] == "emscripten-link"]
    assert [row["scenario"] for row in em_link] == [
        "HTML link, Cold",
        "HTML link, Warm",
        "Wasm link, Cold",
        "Wasm link, Warm",
    ]

    rust_link = [row for row in rows if row["benchmark"] == "rust-workspace-link"]
    assert [row["mode"] for row in rust_link] == ["cold", "warm"]
    assert rust_link[1]["scenario"] == "Workspace staticlib link, Warm"
    assert all(isinstance(row["bare_cache_bytes"], int) for row in rows)
    assert all(isinstance(row["sccache_cache_bytes"], int) for row in rows)
    assert all(isinstance(row["zccache_cache_bytes"], int) for row in rows)
    assert all(row["cache_bytes_reported"] is False for row in rows)


def test_parse_benchmark_log_extracts_cache_bytes_from_new_table_shape():
    log = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | bare cache | sccache cache | zccache cache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|--------------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | 3.200s | 2.000s | 0 B | 12.5 KiB | 2.0 MiB | 1.6x faster | 1.5x faster |
"""

    row = benchmark_stats.parse_benchmark_log(log)[0]

    assert row["bare_cache_bytes"] == 0
    assert row["sccache_cache_bytes"] == 12800
    assert row["zccache_cache_bytes"] == 2 * 1024 * 1024
    assert row["cache_bytes_reported"] is True
    assert row["zccache_vs_sccache_ratio"] == 1.6
    assert row["zccache_vs_bare_ratio"] == 1.5


def test_write_outputs_appends_history_with_cache_fields(tmp_path):
    payload = sample_payload()
    payload["results"][0]["sccache_cache_bytes"] = 123
    payload["results"][0]["zccache_cache_bytes"] = 456

    benchmark_stats.write_outputs(payload, tmp_path)

    rows = [
        json.loads(line)
        for line in (tmp_path / "history.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    assert len(rows) == 1
    assert rows[0]["sha"] == "abcdef1234567890"
    assert rows[0]["results"][0]["bare_cache_bytes"] == 0
    assert rows[0]["results"][0]["sccache_cache_bytes"] == 123
    assert rows[0]["results"][0]["zccache_cache_bytes"] == 456


def test_benchmark_env_enables_cc_miss_profile(tmp_path, monkeypatch):
    # Issue #535: the non-rust cold-miss profile (`zccache_cc_miss_profile`
    # in `handle_compile/miss_profile.rs`) must be set in the published-log
    # bench env so c-static-library-link / cpp-driver-link / emscripten
    # cold rows carry phase data — the prerequisite for the #535 perf fix.
    monkeypatch.setenv("RUSTC_WRAPPER", "sccache")

    env = benchmark_stats.benchmark_env(tmp_path)

    assert env["ZCCACHE_PROFILE_CC_MISS"] == "1"


def test_benchmark_env_enables_rust_miss_profile(tmp_path, monkeypatch):
    # Issue #517: benchmark-stats publishes one log per run to the
    # `benchmark-stats` branch. The log must include `zccache_rust_miss_profile`
    # lines so future perf investigations can read the cold-path phase
    # breakdown directly from the published artifact, without having to
    # re-run the bench manually. `ZCCACHE_PROFILE_RUST_MISS` is the env knob
    # the daemon already keys on (see `RUST_MISS_PROFILE_ENV` in
    # `handle_compile/pipeline.rs`). Mirrors what `perf_guard._benchmark_env`
    # has been doing all along.
    monkeypatch.setenv("RUSTC_WRAPPER", "sccache")

    env = benchmark_stats.benchmark_env(tmp_path)

    assert env["ZCCACHE_CACHE_DIR"] == str(tmp_path)
    assert env["ZCCACHE_PROFILE_RUST_MISS"] == "1"
    assert "RUSTC_WRAPPER" not in env


def test_benchmark_command_targets_existing_workspace_package():
    assert benchmark_stats.BENCHMARK_BASE_COMMAND == [
        "soldr",
        "--no-cache",
        "cargo",
        "test",
        "-p",
        "zccache",
        "--test",
        "perf_bench_test",
    ]


def test_zero_duration_cells_are_invalid_not_inflated():
    # #443: a 0.000s reading is a broken measurement — a cold (or even a warm
    # cache-hit) compile/link is never instant. It must be treated as invalid
    # (None), not silently inflated into a tiny value that yields an absurd
    # speedup ratio like the bogus "1797x faster" below.
    log = """
## C Static-Library Link Benchmark: 50 .o inputs, 5 warm trials

| Scenario | Bare ar | sccache | zccache | vs sccache | vs Bare ar |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Static archive, Cold | 0.057s | 0.057s | 0.000s | 1797x faster | 1797x faster |
"""

    row = benchmark_stats.parse_benchmark_log(log)[0]

    assert row["zccache_seconds"] is None
    assert row["zccache_vs_bare_ratio"] is None
    assert row["zccache_vs_sccache_ratio"] is None
    assert benchmark_stats._duration_seconds("0.0ms") is None
    assert benchmark_stats._duration_seconds("0.000s") is None


def test_warm_display_rounded_zero_duration_uses_reported_ratios():
    log = """
## C++ Driver-Link Benchmark: 50 .cpp objects, 5 warm trials

| Scenario | Bare clang++ | sccache | zccache | vs sccache | vs Bare clang++ |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Driver link, Warm | 0.042s | 0.046s | **0.000s** | **94x faster** | **87x faster** |
"""

    row = benchmark_stats.parse_benchmark_log(log)[0]

    expected_duration = round(((0.046 / 94.0) + (0.042 / 87.0)) / 2.0, 6)
    assert row["zccache_seconds"] == pytest.approx(expected_duration)
    assert row["zccache_vs_sccache_ratio"] == 94.0
    assert row["zccache_vs_bare_ratio"] == 87.0


def test_absurd_warm_display_rounded_zero_stays_invalid():
    log = """
## C Static-Library Link Benchmark: 50 .o inputs, 5 warm trials

| Scenario | Bare ar | sccache | zccache | vs sccache | vs Bare ar |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Static archive, Warm | 0.057s | 0.057s | 0.000s | 1797x faster | 1797x faster |
"""

    row = benchmark_stats.parse_benchmark_log(log)[0]

    assert row["zccache_seconds"] is None
    assert row["zccache_vs_bare_ratio"] is None
    assert row["zccache_vs_sccache_ratio"] is None


def test_group_results_by_language_returns_expected_buckets():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)

    groups = benchmark_stats.group_results_by_language(rows)

    assert list(groups) == ["c", "c++", "emscripten", "rust"]
    assert {benchmark_stats.LANGUAGE_LABELS[language] for language in groups} == {
        "C",
        "C++",
        "Emscripten",
        "Rust",
    }
    assert [len(groups[language]) for language in groups] == [4, 8, 7, 5]


def test_render_html_links_json_stats_and_language_images_only():
    payload = sample_payload()

    html = benchmark_stats.render_html(payload)
    jpg_references = re.findall(r'(?:href|src)="([^"]+\.jpg)"', html)

    assert 'href="latest.json"' in html
    assert set(jpg_references) == set(benchmark_stats.LANGUAGE_IMAGE_FILES.values())
    assert "benchmark.jpg" not in html
    assert "Rust rustc" in html


def test_image_rows_cover_c_cpp_and_rust_stats():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)
    image_rows = benchmark_stats.build_image_rows(rows)

    assert {row["language"] for row in image_rows} == {
        "C",
        "C++",
        "Emscripten",
        "Rust",
    }
    assert any("C inline args - Single-file, Warm" == row["scenario"] for row in image_rows)
    assert any(
        row["compact_label"] == "inline args"
        and row["compact_scenario"] == "Single-file"
        for row in image_rows
    )
    assert any(
        "C++ response files - Single-file RSP, Cold" == row["scenario"]
        for row in image_rows
    )
    assert any(
        row["compact_label"] == "response files"
        and row["compact_scenario"] == "Single-file RSP"
        for row in image_rows
    )
    assert any("Rust rustc - Build, Warm" == row["scenario"] for row in image_rows)
    assert any(
        "C static-library link - Static archive, Warm" == row["scenario"]
        for row in image_rows
    )
    assert any("C++ driver link - Driver link, Warm" == row["scenario"] for row in image_rows)
    assert any("Emscripten link - Wasm link, Warm" == row["scenario"] for row in image_rows)
    assert any(
        "Rust workspace link - Workspace staticlib link, Warm" == row["scenario"]
        for row in image_rows
    )


def test_write_outputs_creates_timestamped_benchmark_image(tmp_path):
    pytest.importorskip("PIL")
    payload = sample_payload()
    stale_combined_image = tmp_path / "benchmark.jpg"
    stale_combined_image.write_text("stale", encoding="utf-8")

    benchmark_stats.write_outputs(payload, tmp_path)

    assert (tmp_path / "latest.json").is_file()
    assert (tmp_path / "index.html").is_file()
    assert (tmp_path / ".nojekyll").is_file()
    for image_file in benchmark_stats.LANGUAGE_IMAGE_FILES.values():
        assert (tmp_path / image_file).stat().st_size > 0
    assert not stale_combined_image.exists()


def test_render_language_jpg_draws_combined_cold_warm_overlay(tmp_path, monkeypatch):
    """Issue #667 (revised): cold and warm collapse into one chart per scenario.

    The image must:
    - draw per-series rows that name the actual compiler ("bare clang",
      "bare clang++") rather than just "bare"
    - emit value labels prefixed with both "cold" and "warm"
    - tell the reader the cold maximum is the 100% reference
    - NOT contain the legacy section-header style ("Cold"/"Warm" as standalone
      titles); we only allow the lowercase value-label form
    - NOT contain "% faster" / "% slower" delta strings (those belong only to
      `index.html` and `latest.json`)
    """
    pytest.importorskip("PIL")

    from PIL import ImageDraw

    captured_text: list[str] = []
    original_text = ImageDraw.ImageDraw.text

    def recording_text(self, xy, text, *args, **kwargs):
        captured_text.append(text)
        return original_text(self, xy, text, *args, **kwargs)

    monkeypatch.setattr(ImageDraw.ImageDraw, "text", recording_text)

    payload = sample_payload()
    out_path = tmp_path / "benchmark-cpp.jpg"
    benchmark_stats.render_language_jpg(payload, "c++", out_path)

    assert out_path.stat().st_size > 0
    joined = "\n".join(captured_text)
    assert "bare clang" in joined
    assert "sccache" in joined
    assert "zccache" in joined
    assert any(t.startswith("cold ") for t in captured_text)
    assert any(t.startswith("warm ") for t in captured_text)
    assert any("100% = cold max" in t for t in captured_text)
    # No standalone section headers from the old layout.
    assert "Cold" not in captured_text
    assert "Warm" not in captured_text
    assert "% faster" not in joined
    assert "% slower" not in joined


def test_strip_mode_suffix_handles_cold_warm_and_canonical_forms():
    f = benchmark_stats._strip_mode_suffix
    assert f("Single-file, Cold") == "Single-file"
    assert f("Single-file, Warm") == "Single-file"
    assert f("Build, Cold") == "Build"
    # Mixed case still strips.
    assert f("Build, cold") == "Build"
    # Names that embed __FILE__ should keep their internals.
    assert (
        f("Sibling-workspace no __FILE__, Warm") == "Sibling-workspace no __FILE__"
    )
    # No marker -> unchanged.
    assert f("Already-canonical scenario") == "Already-canonical scenario"
    # Empty -> empty.
    assert f("") == ""


def test_build_combined_image_rows_merges_cold_and_warm_into_one_entry():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)
    combined = benchmark_stats.build_combined_image_rows(rows)

    by_key = {(c["benchmark"], c["scenario_root"]): c for c in combined}

    # rustc Build merges its Cold and Warm rows into a single entry.
    rustc_build = by_key[("rust", "Build")]
    assert rustc_build["cold"]["bare"] == 8.165
    assert rustc_build["cold"]["sccache"] == 9.634
    assert rustc_build["cold"]["zccache"] == 41.624
    assert rustc_build["warm"]["bare"] == 7.018
    assert rustc_build["warm"]["sccache"] == 8.236
    assert rustc_build["warm"]["zccache"] == 0.123
    # Warm-only scenario should have None for all cold series.
    sibling = by_key[("rust-sibling-remap", "Sibling-workspace")]
    assert set(sibling["cold"].values()) == {None}
    assert sibling["warm"]["zccache"] == 0.127
    # bare_label propagates so the per-row label renders the actual compiler.
    assert rustc_build["bare_label"] == "Bare rustc"
    # No duplicate keys per (benchmark, scenario root).
    keys = [(c["benchmark"], c["scenario_root"]) for c in combined]
    assert len(keys) == len(set(keys))


def test_normalization_reference_prefers_cold_max_then_warm_then_fallback():
    combined = {
        "cold": {"bare": 2.0, "sccache": 3.0, "zccache": 1.0},
        "warm": {"bare": 0.5, "sccache": 0.4, "zccache": 0.01},
    }
    assert benchmark_stats._normalization_reference(combined) == (3.0, "cold")

    warm_only = {
        "cold": {"bare": None, "sccache": None, "zccache": None},
        "warm": {"bare": 1.0, "sccache": 1.5, "zccache": 0.05},
    }
    assert benchmark_stats._normalization_reference(warm_only) == (1.5, "warm")

    empty = {
        "cold": {"bare": None, "sccache": None, "zccache": None},
        "warm": {"bare": None, "sccache": None, "zccache": None},
    }
    assert benchmark_stats._normalization_reference(empty) == (1.0, "none")

    # Zero/negative cold values are filtered out, not used as a divisor.
    only_zero_cold = {
        "cold": {"bare": 0.0, "sccache": -1.0, "zccache": None},
        "warm": {"bare": 0.5, "sccache": None, "zccache": None},
    }
    assert benchmark_stats._normalization_reference(only_zero_cold) == (0.5, "warm")


def test_warm_violations_flags_cache_regression_beyond_margin():
    combined = {
        "cold": {"bare": 1.000, "sccache": 1.000, "zccache": 1.000},
        # bare: under 10% slower -> NOT flagged (noise floor).
        # sccache: 50% slower -> flagged.
        # zccache: faster -> NOT flagged.
        "warm": {"bare": 1.050, "sccache": 1.500, "zccache": 0.050},
    }
    flags = benchmark_stats._warm_violations(combined)
    assert "sccache" in flags
    assert "bare" not in flags
    assert "zccache" not in flags
    assert "1.500s" in flags["sccache"]
    assert "1.000s" in flags["sccache"]


def test_warm_violations_skips_missing_or_zero_values():
    combined = {
        "cold": {"bare": None, "sccache": 1.0, "zccache": 1.0},
        "warm": {"bare": 5.0, "sccache": None, "zccache": 0.0},
    }
    # No pair has both cold > 0 and warm > 0, so no violations.
    assert benchmark_stats._warm_violations(combined) == {}


def test_render_language_jpg_survives_pathological_inputs(tmp_path):
    """A scenario row missing every duration must still draw without crashing."""
    pytest.importorskip("PIL")

    payload = sample_payload()
    payload["results"].append(
        {
            "benchmark": "c-degenerate",
            "benchmark_label": "C degenerate",
            "language": "c",
            "scenario": "All-missing, Warm",
            "mode": "warm",
            "bare_label": "Bare clang",
            "bare_seconds": None,
            "sccache_seconds": None,
            "zccache_seconds": None,
            "zccache_vs_sccache_ratio": None,
            "zccache_vs_bare_ratio": None,
            "vs_sccache_text": "n/a",
            "vs_bare_text": "n/a",
        }
    )
    out_path = tmp_path / "benchmark-c.jpg"
    benchmark_stats.render_language_jpg(payload, "c", out_path)
    assert out_path.stat().st_size > 0


def test_render_language_jpg_handles_no_data_for_language(tmp_path):
    """Empty language slice draws the placeholder, not a crash."""
    pytest.importorskip("PIL")

    payload = sample_payload()
    payload["results"] = [row for row in payload["results"] if row["language"] != "c"]

    out_path = tmp_path / "benchmark-c-empty.jpg"
    benchmark_stats.render_language_jpg(payload, "c", out_path)
    assert out_path.stat().st_size > 0


def test_readme_benchmark_images_link_to_results_branch():
    readme = (benchmark_stats.REPO_ROOT / "README.md").read_text(encoding="utf-8")

    for language, image_file in benchmark_stats.LANGUAGE_IMAGE_FILES.items():
        label = benchmark_stats.LANGUAGE_LABELS[language]
        image_url = f"{benchmark_stats.DEFAULT_RAW_IMAGE_BASE_URL}/{image_file}"
        expected = (
            f"[![Latest zccache {label} benchmark stats]"
            f"({image_url})]({benchmark_stats.BENCHMARK_STATS_BRANCH_URL})"
        )
        assert expected in readme
    assert "benchmark.jpg" not in readme


def test_ratio_tone_and_color_classify_faster_slower_and_neutral():
    assert benchmark_stats.ratio_tone(1.25) == "faster"
    assert benchmark_stats.ratio_color(1.25) == benchmark_stats.RATIO_COLORS["faster"]
    assert benchmark_stats.ratio_tone(0.75) == "slower"
    assert benchmark_stats.ratio_color(0.75) == benchmark_stats.RATIO_COLORS["slower"]
    assert benchmark_stats.ratio_tone(None) == "neutral"
    assert benchmark_stats.ratio_tone(1.0) == "neutral"
    assert benchmark_stats.ratio_color(None) == benchmark_stats.RATIO_COLORS["neutral"]


def test_percent_delta_format_covers_faster_slower_and_missing():
    assert benchmark_stats._format_percent_delta(1.25) == "25.0% faster"
    assert benchmark_stats._format_percent_delta(0.8) == "25.0% slower"
    assert benchmark_stats._format_percent_delta(1.0) == "0.0%"
    assert benchmark_stats._format_percent_delta(None) == "n/a"
