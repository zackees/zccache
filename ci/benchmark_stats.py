"""Generate durable zccache benchmark stats artifacts.

The benchmark runner reuses `perf_bench_test` and turns its markdown tables into
generated files suitable for publishing from an orphan branch:

- `index.html` for humans
- `latest.json` for machines
- `benchmark-c.jpg`, `benchmark-cpp.jpg`, and `benchmark-rust.jpg` for README embedding
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from html import escape
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "benchmark-stats"
DEFAULT_PAGES_URL = "https://zackees.github.io/zccache/"
DEFAULT_RAW_IMAGE_BASE_URL = "https://raw.githubusercontent.com/zackees/zccache/benchmark-stats"
BENCHMARK_STATS_BRANCH_URL = "https://github.com/zackees/zccache/tree/benchmark-stats"
HISTORY_MAX_LINES = 1000
LANGUAGES = ("c", "c++", "emscripten", "rust")
LANGUAGE_LABELS = {
    "c": "C",
    "c++": "C++",
    "emscripten": "Emscripten",
    "rust": "Rust",
}
LANGUAGE_IMAGE_FILES = {
    "c": "benchmark-c.jpg",
    "c++": "benchmark-cpp.jpg",
    "emscripten": "benchmark-emscripten.jpg",
    "rust": "benchmark-rust.jpg",
}
RATIO_COLORS = {
    "faster": "#3fb950",
    "slower": "#f85149",
    "neutral": "#8b949e",
}
# Display-rounded zeroes above this speedup are still suspicious enough to
# treat as missing data. This preserves the #443 guard against broken timing
# while allowing legitimate sub-millisecond warm cache hits such as 87x-96x.
MAX_DISPLAY_ROUNDED_ZERO_RATIO = 1000.0
BENCHMARK_BASE_COMMAND = [
    "soldr",
    "--no-cache",
    "cargo",
    "test",
    "-p",
    "zccache",
    "--test",
    "perf_bench_test",
]
BENCHMARK_TESTS_BY_LANGUAGE = {
    "c": (
        "perf_c_zccache_vs_bare",
        "perf_c_archive_link",
    ),
    "c++": (
        "perf_warm_cache_zccache_vs_sccache",
        "perf_response_file",
        "perf_cpp_sibling_remap_warm",
        "perf_cpp_driver_link",
    ),
    "emscripten": (
        "perf_emcc_warm_cache_zccache_vs_sccache",
        "perf_emcc_sibling_remap_warm",
        "perf_emcc_link",
    ),
    "rust": (
        "perf_rustc_zccache_vs_sccache",
        "perf_rustc_sibling_remap_warm",
        "perf_rust_workspace_link",
    ),
}
BENCHMARK_COMMAND = [
    *BENCHMARK_BASE_COMMAND,
    "--",
    "--nocapture",
    "--ignored",
    "--test-threads=1",
]


TABLES = {
    "## C Benchmark:": {
        "id": "c-inline",
        "label": "C inline args",
        "language": "c",
        "bare_label": "Bare clang",
    },
    "## C Static-Library Link Benchmark:": {
        "id": "c-static-library-link",
        "label": "C static-library link",
        "language": "c",
        "bare_label": "Bare ar",
    },
    "## Benchmark:": {
        "id": "cpp-inline",
        "label": "C++ inline args",
        "language": "c++",
        "bare_label": "Bare clang",
    },
    "## Response-File Benchmark:": {
        "id": "cpp-response-file",
        "label": "C++ response files",
        "language": "c++",
        "bare_label": "Bare clang",
    },
    "## C++ Sibling-Workspace Remap Benchmark:": {
        "id": "cpp-sibling-remap",
        "label": "C++ sibling git remap",
        "language": "c++",
        "bare_label": "Bare clang",
    },
    "## C++ Driver-Link Benchmark:": {
        "id": "cpp-driver-link",
        "label": "C++ driver link",
        "language": "c++",
        "bare_label": "Bare clang++",
    },
    "## Emscripten Benchmark:": {
        "id": "emscripten",
        "label": "Emscripten em++",
        "language": "emscripten",
        "bare_label": "Bare em++",
    },
    "## Emscripten Sibling-Workspace Remap Benchmark:": {
        "id": "emscripten-sibling-remap",
        "label": "Emscripten sibling git remap",
        "language": "emscripten",
        "bare_label": "Bare em++",
    },
    "## Emscripten Link Benchmark:": {
        "id": "emscripten-link",
        "label": "Emscripten link",
        "language": "emscripten",
        "bare_label": "Bare em++",
    },
    "## Rust Benchmark:": {
        "id": "rust",
        "label": "Rust rustc",
        "language": "rust",
        "bare_label": "Bare rustc",
    },
    "## Rust Sibling-Workspace Remap Benchmark:": {
        "id": "rust-sibling-remap",
        "label": "Rust sibling git remap",
        "language": "rust",
        "bare_label": "Bare rustc",
    },
    "## Rust Workspace Link Benchmark:": {
        "id": "rust-workspace-link",
        "label": "Rust workspace link",
        "language": "rust",
        "bare_label": "Bare rustc",
    },
}


def benchmark_command_for_test(test_name: str) -> list[str]:
    return [
        *BENCHMARK_BASE_COMMAND,
        "--",
        test_name,
        "--nocapture",
        "--ignored",
        "--test-threads=1",
    ]


def benchmark_commands_for_language(language: str) -> list[list[str]]:
    try:
        test_names = BENCHMARK_TESTS_BY_LANGUAGE[language]
    except KeyError as exc:
        supported = ", ".join(sorted(BENCHMARK_TESTS_BY_LANGUAGE))
        message = f"unsupported benchmark language {language!r}; expected {supported}"
        raise ValueError(message) from exc
    return [benchmark_command_for_test(test_name) for test_name in test_names]


def _utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def _run_quiet(cmd: list[str], cwd: Path = REPO_ROOT) -> str | None:
    try:
        result = subprocess.run(
            cmd,
            cwd=cwd,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    except OSError:
        return None
    output = result.stdout.strip()
    if result.returncode != 0:
        return output or None
    return output or None


def _first_line(value: str | None) -> str | None:
    if not value:
        return None
    for line in value.splitlines():
        line = line.strip()
        if line:
            return line
    return None


def _git_sha() -> str | None:
    return os.environ.get("GITHUB_SHA") or _first_line(_run_quiet(["git", "rev-parse", "HEAD"]))


def _git_ref() -> str | None:
    return os.environ.get("GITHUB_REF_NAME") or _first_line(
        _run_quiet(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    )


def collect_metadata() -> dict[str, Any]:
    run_id = os.environ.get("GITHUB_RUN_ID")
    repository = os.environ.get("GITHUB_REPOSITORY", "zackees/zccache")
    raw_image_base_url = os.environ.get(
        "ZCCACHE_BENCHMARK_RAW_BASE_URL", DEFAULT_RAW_IMAGE_BASE_URL
    ).rstrip("/")
    run_url = None
    if run_id:
        run_url = f"https://github.com/{repository}/actions/runs/{run_id}"

    return {
        "generated_at": _utc_now(),
        "repository": repository,
        "git_sha": _git_sha(),
        "git_ref": _git_ref(),
        "run_url": run_url,
        "runner": {
            "os": os.environ.get("RUNNER_OS") or platform.system(),
            "arch": platform.machine(),
            "platform": platform.platform(),
            "cpu_count": os.cpu_count(),
        },
        "versions": {
            "soldr": _first_line(_run_quiet(["soldr", "version"])),
            "rustc": _first_line(_run_quiet(["soldr", "--no-cache", "rustc", "--version"])),
            "clang": _first_line(_run_quiet(["clang++", "--version"])),
            "sccache": _first_line(_run_quiet(["sccache", "--version"])),
        },
        "benchmark_command": " ".join(BENCHMARK_COMMAND),
        "pages_url": os.environ.get("ZCCACHE_BENCHMARK_PAGES_URL", DEFAULT_PAGES_URL),
        "raw_image_base_url": raw_image_base_url,
        "raw_image_urls": {
            language: f"{raw_image_base_url}/{image_file}"
            for language, image_file in LANGUAGE_IMAGE_FILES.items()
        },
    }


def benchmark_env(cache_dir: Path) -> dict[str, str]:
    """Env used by `run_benchmarks` to invoke the bench binary.

    Mirrors `ci.perf_guard._benchmark_env` for the rust-relevant bits so the
    nightly benchmark-stats run captures the same per-phase
    `zccache_rust_miss_profile` lines perf_guard already emits — issue #517
    needs that breakdown in the published bench log to identify which phase
    of the rust-workspace-link Cold path owns the 91 ms of overhead.
    """
    env = os.environ.copy()
    env["ZCCACHE_CACHE_DIR"] = str(cache_dir)
    env["ZCCACHE_PROFILE_RUST_MISS"] = "1"
    # Issue #535: emit the non-rustc cold-miss profile too. The daemon
    # gates on this env independently of the rust one, so both the
    # `zccache_rust_miss_profile` and `zccache_cc_miss_profile` lines
    # land in the published bench log — letting future investigations
    # read C/C++ link-path phase data without re-running the bench.
    env["ZCCACHE_PROFILE_CC_MISS"] = "1"
    env.pop("RUSTC_WRAPPER", None)
    return env


def run_benchmarks(log_path: Path) -> str:
    cache_dir = Path(tempfile.mkdtemp(prefix="zccache-benchmark-cache-"))
    env = benchmark_env(cache_dir)

    try:
        result = subprocess.run(
            BENCHMARK_COMMAND,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    finally:
        shutil.rmtree(cache_dir, ignore_errors=True)

    output = result.stdout
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(output, encoding="utf-8")
    print(output, end="")
    if result.returncode != 0:
        raise SystemExit(result.returncode)
    return output


def _clean_cell(cell: str) -> str:
    text = cell.strip()
    text = text.replace("**", "")
    text = text.replace("`", "")
    return text.strip()


def _duration_seconds(value: str) -> float | None:
    text = _clean_cell(value)
    if not text or text in {"-", "\u2014", "n/a", "N/A"}:
        return None
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)\s*(ms|s)", text)
    if match is None:
        return None
    number = float(match.group(1))
    unit = match.group(2)
    if number == 0.0:
        # #443: a reading that rounds to 0 is not a real measurement — a cold
        # (or even warm cache-hit) compile/link is never instant; only a broken
        # measurement (e.g. timing the IPC send instead of the daemon
        # round-trip) reads 0.000s. Return None instead of silently inflating
        # it to a tiny value, so it can't feed an absurd speedup ratio.
        return None
    if unit == "ms":
        return round(number / 1000.0, 6)
    return round(number, 6)


def _bytes_from_text(value: str) -> int | None:
    text = _clean_cell(value)
    if not text or text in {"-", "\u2014", "n/a", "N/A"}:
        return None
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)\s*(B|KiB|MiB|GiB|KB|MB|GB)", text)
    if match is None:
        return None
    number = float(match.group(1))
    unit = match.group(2)
    factors = {
        "B": 1,
        "KiB": 1024,
        "MiB": 1024**2,
        "GiB": 1024**3,
        "KB": 1000,
        "MB": 1000**2,
        "GB": 1000**3,
    }
    return int(round(number * factors[unit]))


def _ratio_from_text(value: str) -> float | None:
    text = _clean_cell(value)
    if text == "~same":
        return 1.0
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)x\s+(faster|slower)", text)
    if match is None:
        return None
    number = float(match.group(1))
    if number <= 0:
        return None
    if match.group(2) == "slower":
        return round(1.0 / number, 3)
    return round(number, 3)


def _is_display_rounded_zero(value: str) -> bool:
    text = _clean_cell(value)
    match = re.fullmatch(r"0+(?:\.0+)?\s*(ms|s)", text)
    return match is not None


def _duration_from_display_rounded_zero(
    mode: str,
    zccache_cell: str,
    comparisons: tuple[tuple[float | None, str], ...],
) -> float | None:
    if mode != "warm" or not _is_display_rounded_zero(zccache_cell):
        return None

    candidates: list[float] = []
    for baseline_seconds, ratio_text in comparisons:
        ratio = _ratio_from_text(ratio_text)
        if ratio is None or ratio <= 1.0:
            continue
        if ratio > MAX_DISPLAY_ROUNDED_ZERO_RATIO:
            return None
        if baseline_seconds is not None:
            candidates.append(baseline_seconds / ratio)

    if not candidates:
        return None
    return round(sum(candidates) / len(candidates), 6)


def _ratio(baseline: float | None, candidate: float | None) -> float | None:
    if baseline is None or candidate is None or candidate <= 0:
        return None
    return round(baseline / candidate, 3)


def parse_benchmark_log(text: str) -> list[dict[str, Any]]:
    current: dict[str, str] | None = None
    results: list[dict[str, Any]] = []

    for raw_line in text.splitlines():
        line = raw_line.strip()
        for prefix, table in TABLES.items():
            if line.startswith(prefix):
                current = table
                break
        else:
            if current is None or not line.startswith("|"):
                continue
            cells = [_clean_cell(cell) for cell in line.strip("|").split("|")]
            if len(cells) < 6 or cells[0] == "Scenario" or set(cells[0]) <= {":", "-"}:
                continue

            bare_seconds = _duration_seconds(cells[1])
            sccache_seconds = _duration_seconds(cells[2])
            zccache_seconds = _duration_seconds(cells[3])
            if len(cells) >= 9:
                bare_cache_bytes = _bytes_from_text(cells[4])
                sccache_cache_bytes = _bytes_from_text(cells[5])
                zccache_cache_bytes = _bytes_from_text(cells[6])
                vs_sccache_cell = cells[7]
                vs_bare_cell = cells[8]
            else:
                bare_cache_bytes = 0
                sccache_cache_bytes = 0
                zccache_cache_bytes = 0
                vs_sccache_cell = cells[4]
                vs_bare_cell = cells[5]
            cache_bytes_reported = len(cells) >= 9
            scenario = cells[0]
            mode = "warm" if "warm" in scenario.lower() else "cold"
            zccache_rounded_to_zero = zccache_seconds is None and _is_display_rounded_zero(
                cells[3]
            )
            if zccache_rounded_to_zero:
                zccache_seconds = _duration_from_display_rounded_zero(
                    mode,
                    cells[3],
                    ((sccache_seconds, vs_sccache_cell), (bare_seconds, vs_bare_cell)),
                )

            sccache_ratio = _ratio(sccache_seconds, zccache_seconds)
            bare_ratio = _ratio(bare_seconds, zccache_seconds)
            if zccache_seconds is not None and zccache_rounded_to_zero:
                sccache_ratio = _ratio_from_text(vs_sccache_cell) or sccache_ratio
                bare_ratio = _ratio_from_text(vs_bare_cell) or bare_ratio
            result = {
                "benchmark": current["id"],
                "benchmark_label": current["label"],
                "language": current["language"],
                "scenario": scenario,
                "mode": mode,
                "bare_label": current["bare_label"],
                "bare_seconds": bare_seconds,
                "sccache_seconds": sccache_seconds,
                "zccache_seconds": zccache_seconds,
                "bare_cache_bytes": bare_cache_bytes if bare_cache_bytes is not None else 0,
                "sccache_cache_bytes": sccache_cache_bytes if sccache_cache_bytes is not None else 0,
                "zccache_cache_bytes": zccache_cache_bytes if zccache_cache_bytes is not None else 0,
                "cache_bytes_reported": cache_bytes_reported,
                "zccache_vs_sccache_ratio": sccache_ratio,
                "zccache_vs_bare_ratio": bare_ratio,
                "vs_sccache_text": vs_sccache_cell,
                "vs_bare_text": vs_bare_cell,
            }
            results.append(result)

    return results


def _format_seconds(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.3f}s"


def _format_bytes(value: int | float | None) -> str:
    if not isinstance(value, (int, float)) or value < 0:
        return "n/a"
    units = ("B", "KiB", "MiB", "GiB")
    amount = float(value)
    unit = units[0]
    for unit in units:
        if amount < 1024 or unit == units[-1]:
            break
        amount /= 1024
    if unit == "B":
        return f"{int(amount)} B"
    if amount < 10:
        return f"{amount:.1f} {unit}"
    return f"{amount:.0f} {unit}"


def _format_ratio(value: float | None) -> str:
    if value is None:
        return "n/a"
    if value >= 1:
        return f"{value:.1f}x faster"
    return f"{1 / value:.1f}x slower"


def _format_percent_delta(value: float | None) -> str:
    if value is None:
        return "n/a"
    if value == 1:
        return "0.0%"
    if value > 1:
        return f"{(value - 1) * 100:.1f}% faster"
    return f"{(1 / value - 1) * 100:.1f}% slower"


def ratio_tone(value: float | None) -> str:
    if value is None or value == 1:
        return "neutral"
    if value > 1:
        return "faster"
    return "slower"


def ratio_color(value: float | None) -> str:
    return RATIO_COLORS[ratio_tone(value)]


def build_summary(results: list[dict[str, Any]]) -> dict[str, Any]:
    warm = [row for row in results if row["mode"] == "warm"]
    cold = [row for row in results if row["mode"] == "cold"]
    best_vs_sccache = max(
        (row for row in warm if row["zccache_vs_sccache_ratio"] is not None),
        key=lambda row: row["zccache_vs_sccache_ratio"],
        default=None,
    )
    best_vs_bare = max(
        (row for row in warm if row["zccache_vs_bare_ratio"] is not None),
        key=lambda row: row["zccache_vs_bare_ratio"],
        default=None,
    )
    return {
        "row_count": len(results),
        "warm_row_count": len(warm),
        "cold_row_count": len(cold),
        "best_warm_vs_sccache": best_vs_sccache,
        "best_warm_vs_bare": best_vs_bare,
    }


def build_payload(results: list[dict[str, Any]], metadata: dict[str, Any]) -> dict[str, Any]:
    return {
        "schema_version": 2,
        "metadata": metadata,
        "summary": build_summary(results),
        "results": results,
    }


def build_history_row(payload: dict[str, Any]) -> dict[str, Any]:
    metadata = payload["metadata"]
    return {
        "ts": metadata.get("generated_at"),
        "sha": metadata.get("git_sha"),
        "summary": payload.get("summary"),
        "results": [
            {
                "benchmark": row.get("benchmark"),
                "language": row.get("language"),
                "scenario": row.get("scenario"),
                "mode": row.get("mode"),
                "bare_seconds": row.get("bare_seconds"),
                "sccache_seconds": row.get("sccache_seconds"),
                "zccache_seconds": row.get("zccache_seconds"),
                "bare_cache_bytes": row.get("bare_cache_bytes"),
                "sccache_cache_bytes": row.get("sccache_cache_bytes"),
                "zccache_cache_bytes": row.get("zccache_cache_bytes"),
                "cache_bytes_reported": row.get("cache_bytes_reported"),
            }
            for row in payload.get("results", [])
        ],
    }


def write_history_jsonl(payload: dict[str, Any], output_dir: Path) -> None:
    history_path = output_dir / "history.jsonl"
    rows: list[str] = []
    if history_path.exists():
        rows = [
            line
            for line in history_path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
    rows = rows[-(HISTORY_MAX_LINES - 1) :]
    rows.append(json.dumps(build_history_row(payload), separators=(",", ":")))
    history_path.write_text("\n".join(rows) + "\n", encoding="utf-8")


def _grouped_results(results: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    groups: dict[str, list[dict[str, Any]]] = {}
    for result in results:
        groups.setdefault(result["benchmark_label"], []).append(result)
    return groups


def group_results_by_language(results: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    return {
        language: [row for row in results if row["language"] == language]
        for language in LANGUAGES
    }


def render_html(payload: dict[str, Any]) -> str:
    metadata = payload["metadata"]
    rows_html: list[str] = []
    for group, rows in _grouped_results(payload["results"]).items():
        rows_html.append(
            "<tr class=\"group\"><th colspan=\"9\">" + escape(group) + "</th></tr>"
        )
        for row in rows:
            rows_html.append(
                "<tr>"
                f"<td>{escape(row['scenario'])}</td>"
                f"<td>{escape(_format_seconds(row['bare_seconds']))}</td>"
                f"<td>{escape(_format_seconds(row['sccache_seconds']))}</td>"
                f"<td class=\"strong\">{escape(_format_seconds(row['zccache_seconds']))}</td>"
                f"<td>{escape(_format_bytes(row.get('bare_cache_bytes')))}</td>"
                f"<td>{escape(_format_bytes(row.get('sccache_cache_bytes')))}</td>"
                f"<td>{escape(_format_bytes(row.get('zccache_cache_bytes')))}</td>"
                f"<td>{escape(_format_ratio(row['zccache_vs_sccache_ratio']))}</td>"
                f"<td>{escape(_format_ratio(row['zccache_vs_bare_ratio']))}</td>"
                "</tr>"
            )

    versions = metadata["versions"]
    version_items = "\n".join(
        f"<li><strong>{escape(name)}</strong>: {escape(value or 'n/a')}</li>"
        for name, value in versions.items()
    )
    run_link = ""
    if metadata.get("run_url"):
        run_link = f" | <a href=\"{escape(metadata['run_url'])}\">workflow run</a>"

    image_links = "\n".join(
        f'<li><a href="{escape(image_file)}">{escape(image_file)}</a></li>'
        for image_file in LANGUAGE_IMAGE_FILES.values()
    )
    image_figures = "\n".join(
        "<figure>"
        f'<a href="{escape(image_file)}">'
        f'<img src="{escape(image_file)}" '
        f'alt="Latest zccache {escape(LANGUAGE_LABELS[language])} benchmark stats" />'
        "</a>"
        f"<figcaption>{escape(LANGUAGE_LABELS[language])}</figcaption>"
        "</figure>"
        for language, image_file in LANGUAGE_IMAGE_FILES.items()
    )

    return f"""<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>zccache benchmark stats</title>
    <style>
      body {{
        margin: 0;
        padding: 32px 20px 44px;
        font-family: Arial, sans-serif;
        color: #f0f6fc;
        background: #0d1117;
      }}
      main {{
        max-width: 1080px;
        margin: 0 auto;
      }}
      h1 {{
        margin: 0 0 10px;
        font-size: 34px;
      }}
      h2 {{
        margin: 28px 0 10px;
        font-size: 22px;
      }}
      p, li {{
        line-height: 1.5;
      }}
      .meta {{
        color: #8b949e;
      }}
      .note {{
        padding: 12px 14px;
        background: #161b22;
        border: 1px solid #30363d;
      }}
      a {{
        color: #58a6ff;
      }}
      .images {{
        display: grid;
        gap: 16px;
        grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
        margin: 20px 0;
      }}
      figure {{
        margin: 0;
      }}
      figcaption {{
        color: #8b949e;
        font-size: 13px;
        margin-top: 6px;
      }}
      .table-wrap {{
        overflow-x: auto;
      }}
      table {{
        width: 100%;
        min-width: 820px;
        border-collapse: collapse;
        margin-top: 18px;
        background: #161b22;
      }}
      th, td {{
        border: 1px solid #30363d;
        padding: 10px 12px;
        text-align: left;
        font-size: 14px;
      }}
      thead th, tr.group th {{
        background: #21262d;
      }}
      .strong {{
        font-weight: 700;
      }}
      img {{
        max-width: 100%;
        border: 1px solid #30363d;
      }}
    </style>
  </head>
  <body>
    <main>
      <h1>zccache benchmark stats</h1>
      <p class="meta">
        Generated {escape(metadata['generated_at'])} |
        ref {escape(metadata.get('git_ref') or 'n/a')} |
        sha {escape((metadata.get('git_sha') or 'n/a')[:12])}{run_link}
      </p>
      <p class="note">
        Raw machine-readable data: <a href="latest.json">latest.json</a> and
        <a href="history.jsonl">history.jsonl</a>.
        Per-language README images:
      </p>
      <ul>
        {image_links}
      </ul>
      <div class="images">
        {image_figures}
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Scenario</th>
              <th>Bare compiler</th>
              <th>sccache</th>
              <th>zccache</th>
              <th>bare cache</th>
              <th>sccache cache</th>
              <th>zccache cache</th>
              <th>zccache vs sccache</th>
              <th>zccache vs bare</th>
            </tr>
          </thead>
          <tbody>
            {''.join(rows_html)}
          </tbody>
        </table>
      </div>
      <h2>Environment</h2>
      <ul>
        <li><strong>Runner</strong>: {escape(metadata['runner']['platform'])}</li>
        <li><strong>Benchmark command</strong>: <code>{escape(metadata['benchmark_command'])}</code></li>
        {version_items}
      </ul>
    </main>
  </body>
</html>
"""


def _font(size: int, bold: bool = False) -> Any:
    candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf" if bold else "",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "C:/Windows/Fonts/arialbd.ttf" if bold else "",
        "C:/Windows/Fonts/arial.ttf",
    ]
    for candidate in candidates:
        if candidate and Path(candidate).exists():
            try:
                from PIL import ImageFont

                return ImageFont.truetype(candidate, size)
            except OSError:
                continue
    from PIL import ImageFont

    return ImageFont.load_default()


def _text_width(draw: Any, value: str, font: Any) -> int:
    bbox = draw.textbbox((0, 0), value, font=font)
    return bbox[2] - bbox[0]


def _truncate_to_width(draw: Any, value: str, font: Any, max_width: int) -> str:
    if _text_width(draw, value, font) <= max_width:
        return value
    ellipsis = "..."
    if _text_width(draw, ellipsis, font) > max_width:
        return ""
    while value and _text_width(draw, value.rstrip() + ellipsis, font) > max_width:
        value = value[:-1]
    return value.rstrip() + ellipsis


def _compact_benchmark_label(language: str, label: str) -> str:
    prefix = f"{LANGUAGE_LABELS.get(language, language)} "
    if label.startswith(prefix):
        return label[len(prefix) :]
    return label


def _compact_scenario(value: str) -> str:
    return re.sub(r",\s*(Cold|Warm)\b", "", value)


def build_image_rows(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "language": LANGUAGE_LABELS.get(str(row["language"]), str(row["language"])),
            "scenario": f"{row['benchmark_label']} - {row['scenario']}",
            "compact_label": _compact_benchmark_label(row["language"], row["benchmark_label"]),
            "compact_scenario": _compact_scenario(row["scenario"]),
            "mode": row["mode"],
            "bare": _format_seconds(row["bare_seconds"]),
            "sccache": _format_seconds(row["sccache_seconds"]),
            "zccache": _format_seconds(row["zccache_seconds"]),
            "bare_seconds": row["bare_seconds"],
            "sccache_seconds": row["sccache_seconds"],
            "zccache_seconds": row["zccache_seconds"],
            "vs_sccache": _format_ratio(row["zccache_vs_sccache_ratio"]),
            "vs_bare": _format_ratio(row["zccache_vs_bare_ratio"]),
            "vs_sccache_percent": _format_percent_delta(row["zccache_vs_sccache_ratio"]),
            "vs_bare_percent": _format_percent_delta(row["zccache_vs_bare_ratio"]),
            "zccache_vs_sccache_ratio": row["zccache_vs_sccache_ratio"],
            "zccache_vs_bare_ratio": row["zccache_vs_bare_ratio"],
        }
        for row in results
    ]


SECTION_STYLES: dict[str, dict[str, str]] = {
    "cold": {
        "title": "Cold",
        "section": "#10233a",
        "row": "#0f1b2d",
        "row_alt": "#11243a",
        "accent": "#79c0ff",
    },
    "warm": {
        "title": "Warm",
        "section": "#2f2110",
        "row": "#241a0f",
        "row_alt": "#2b1f12",
        "accent": "#ffa657",
    },
}

SERIES_ORDER: tuple[str, ...] = ("bare", "sccache", "zccache")

# Cold/warm color pairs per series. Cold is the muted back layer, warm is the
# vivid foreground that overlays it — the cache hit is the headline, and the
# eye should land on it.  zccache uses GitHub-red so it pops against the
# muted bare/sccache pair.  Issue #667.
SERIES_COLOR_PAIRS: dict[str, dict[str, str]] = {
    "bare":    {"cold": "#3b4046", "warm": "#8b949e"},
    "sccache": {"cold": "#1f3a7a", "warm": "#79c0ff"},
    "zccache": {"cold": "#5b1f1c", "warm": "#f85149"},
}
# Legacy single color (warm shade) — kept so anything still reading
# SERIES_COLORS keeps working with the same visual identity.
SERIES_COLORS: dict[str, str] = {
    series: pair["warm"] for series, pair in SERIES_COLOR_PAIRS.items()
}

WARM_VIOLATION_OUTLINE = "#ffd43b"
# How much slower (relative to cold) warm has to be before we flag it as a
# violation.  Some noise is normal; >10% is meaningful.
WARM_VIOLATION_MARGIN = 1.10


def _section_max_seconds(rows: list[dict[str, Any]]) -> float:
    """Largest finite duration across bare/sccache/zccache for a flat row set."""
    candidates = [
        value
        for row in rows
        for value in (row.get("bare_seconds"), row.get("sccache_seconds"), row.get("zccache_seconds"))
        if isinstance(value, (int, float)) and value > 0
    ]
    if not candidates:
        return 1.0
    return float(max(candidates))


def _section_rows(rows: list[dict[str, Any]], mode: str) -> list[dict[str, Any]]:
    return [row for row in rows if row["mode"] == mode]


_MODE_SUFFIX_RE = re.compile(r",\s*(Cold|Warm)\s*$", re.IGNORECASE)


def _strip_mode_suffix(scenario: str) -> str:
    """Drop a trailing ', Cold' / ', Warm' marker from a scenario label.

    Combining cold and warm rows into one entry needs a stable root key; the
    parser leaves the mode marker baked into the scenario name.  Rows that
    don't carry the marker (already-canonical scenarios) are returned as-is.
    """
    return _MODE_SUFFIX_RE.sub("", scenario).strip()


def build_combined_image_rows(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Merge per-mode rows into one entry per (benchmark, scenario root).

    Each combined entry exposes cold and warm seconds for every series so the
    bar chart can render both bars side-by-side under the same scenario
    heading.  Either side may be absent (warm-only benchmarks are common).
    """
    groups: dict[tuple[str, str], dict[str, Any]] = {}
    order: list[tuple[str, str]] = []
    for row in results:
        scenario_root = _strip_mode_suffix(row.get("scenario", ""))
        key = (row.get("benchmark", ""), scenario_root)
        combined = groups.get(key)
        if combined is None:
            combined = {
                "benchmark": row.get("benchmark", ""),
                "benchmark_label": row.get("benchmark_label", ""),
                "language": row.get("language", ""),
                "bare_label": row.get("bare_label", "Bare"),
                "scenario_root": scenario_root,
                "compact_label": _compact_benchmark_label(
                    str(row.get("language", "")), str(row.get("benchmark_label", ""))
                ),
                "compact_scenario": _compact_scenario(scenario_root),
                "cold": {series: None for series in SERIES_ORDER},
                "warm": {series: None for series in SERIES_ORDER},
                "cache_bytes": {series: None for series in SERIES_ORDER},
            }
            groups[key] = combined
            order.append(key)
        mode = row.get("mode")
        if mode not in ("cold", "warm"):
            continue
        for series in SERIES_ORDER:
            value = row.get(f"{series}_seconds")
            if isinstance(value, (int, float)) and value > 0:
                combined[mode][series] = float(value)
            cache_value = row.get(f"{series}_cache_bytes")
            if isinstance(cache_value, (int, float)) and cache_value >= 0:
                combined["cache_bytes"][series] = int(cache_value)
    return [groups[key] for key in order]


def _normalization_reference(combined_row: dict[str, Any]) -> tuple[float, str]:
    """Return (max_value, source) for normalizing every bar in a scenario.

    Spec (#667): the max cold time is the 100% mark; everything scales against
    it.  When no cold values exist (warm-only scenarios) we fall back to the
    max warm so the bars still render at a useful scale.  Returns (1.0,
    "none") when no usable data is present so callers can divide safely.
    """
    cold_values = [
        v for v in combined_row.get("cold", {}).values()
        if isinstance(v, (int, float)) and v > 0
    ]
    if cold_values:
        return (float(max(cold_values)), "cold")
    warm_values = [
        v for v in combined_row.get("warm", {}).values()
        if isinstance(v, (int, float)) and v > 0
    ]
    if warm_values:
        return (float(max(warm_values)), "warm")
    return (1.0, "none")


def _warm_violations(combined_row: dict[str, Any]) -> dict[str, str]:
    """Series -> human-readable violation reason.

    Cache "regression" = warm visibly slower than cold (>10%).  We don't flag
    parity differences (noise floor), only the kind of warm-too-slow result
    that means something is wrong with the cache for that series.
    """
    flags: dict[str, str] = {}
    cold = combined_row.get("cold", {})
    warm = combined_row.get("warm", {})
    for series in SERIES_ORDER:
        cold_value = cold.get(series)
        warm_value = warm.get(series)
        if not isinstance(cold_value, (int, float)) or cold_value <= 0:
            continue
        if not isinstance(warm_value, (int, float)) or warm_value <= 0:
            continue
        if warm_value > cold_value * WARM_VIOLATION_MARGIN:
            flags[series] = f"warm {warm_value:.3f}s > cold {cold_value:.3f}s"
    return flags


def _format_seconds_label(value: float | None) -> str:
    """Tight value label for the bar annotation. n/a when no datum."""
    if not isinstance(value, (int, float)) or value <= 0:
        return "n/a"
    return f"{value:.3f}s"


def render_language_jpg(payload: dict[str, Any], language: str, path: Path) -> None:
    """Render a per-language benchmark image as combined cold+warm bar charts.

    One section per scenario.  Inside the section, one row per series (bare
    <compiler> / sccache / zccache) showing the cold bar in the back and the
    warm bar overlaid in front, both normalized against the per-scenario
    cold-max (#667).  Warm-only scenarios fall back to warm-max so the bars
    still scale.  Warm-slower-than-cold (>10%) is flagged with an outline.
    """
    try:
        from PIL import Image, ImageDraw
    except ImportError as exc:
        raise SystemExit(
            "Pillow is required to write benchmark JPGs. Install it with "
            "`uv run --with pillow` or `python -m pip install Pillow`."
        ) from exc

    language_results = group_results_by_language(payload["results"])[language]
    combined_rows = build_combined_image_rows(language_results)
    title = f"zccache {LANGUAGE_LABELS[language]} benchmarks"

    width = 900
    margin = 20
    header_band_h = 116
    chart_top = 134
    footer_h = 56
    legend_h = 40

    bar_row_h = 54
    scenario_label_h = 36
    scenario_gap = 16
    scenario_padding_top = 10
    scenario_padding_bottom = 12
    empty_section_h = 90

    def scenario_block_height() -> int:
        return (
            scenario_padding_top
            + scenario_label_h
            + bar_row_h * len(SERIES_ORDER)
            + scenario_padding_bottom
        )

    if combined_rows:
        chart_h = (
            legend_h
            + (scenario_block_height() + scenario_gap) * len(combined_rows)
            - scenario_gap
        )
    else:
        chart_h = legend_h + empty_section_h

    height = max(460, chart_top + chart_h + footer_h)

    scale = 4
    image = Image.new("RGB", (width * scale, height * scale), "#0d1117")
    draw = ImageDraw.Draw(image)

    title_font = _font(32 * scale, bold=True)
    subtitle_font = _font(14 * scale)
    scenario_font = _font(20 * scale, bold=True)
    series_font = _font(17 * scale, bold=True)
    value_font = _font(15 * scale, bold=True)
    legend_font = _font(14 * scale, bold=True)
    small_font = _font(13 * scale)

    def box(values: tuple[int, int, int, int]) -> tuple[int, int, int, int]:
        return (
            values[0] * scale,
            values[1] * scale,
            values[2] * scale,
            values[3] * scale,
        )

    def point(x: int, y: int) -> tuple[int, int]:
        return x * scale, y * scale

    def draw_fit(
        x: int,
        y: int,
        value: str,
        font: Any,
        fill: str,
        max_width: int,
    ) -> None:
        draw.text(
            point(x, y),
            _truncate_to_width(draw, value, font, max_width * scale),
            font=font,
            fill=fill,
        )

    # Header band ------------------------------------------------------------
    draw.rectangle(box((0, 0, width, height)), fill="#0d1117")
    draw.rectangle(box((0, 0, width, header_band_h)), fill="#161b22")
    draw.text(point(margin, 22), title, font=title_font, fill="#f0f6fc")
    metadata = payload["metadata"]
    sha = (metadata.get("git_sha") or "n/a")[:12]
    runner = metadata.get("runner", {}).get("platform") or "n/a"
    metadata_line = (
        f"Generated {metadata['generated_at']} | ref {metadata.get('git_ref') or 'n/a'} | "
        f"sha {sha} | runner {runner}"
    )
    draw_fit(
        margin,
        70,
        metadata_line,
        subtitle_font,
        "#8b949e",
        width - margin * 2,
    )
    draw_fit(
        margin,
        92,
        "Per-scenario bars normalized to the cold maximum (100%); warm overlays on top.",
        subtitle_font,
        "#8b949e",
        width - margin * 2,
    )

    # Chart geometry ---------------------------------------------------------
    x0 = margin
    chart_w = width - margin * 2
    series_label_w = 160
    value_label_w = 200
    inner_padding = 16
    bar_area_x0 = x0 + inner_padding + series_label_w
    bar_area_x1 = x0 + chart_w - inner_padding - value_label_w
    bar_area_w = max(1, bar_area_x1 - bar_area_x0)

    # Legend at the top of the chart area --------------------------------------
    def draw_legend(y: int) -> int:
        legend_top = y + 8
        # Sample swatches: cold (back, full height) + warm (front, narrower)
        swatch_x = x0 + inner_padding
        swatch_w = 60
        cold_top = legend_top
        cold_bottom = legend_top + 20
        warm_top = legend_top + 5
        warm_bottom = legend_top + 15
        # cold sample (uses zccache-cold to telegraph "this is the protagonist")
        draw.rectangle(
            box((swatch_x, cold_top, swatch_x + swatch_w, cold_bottom)),
            fill=SERIES_COLOR_PAIRS["zccache"]["cold"],
        )
        draw.rectangle(
            box((swatch_x, warm_top, swatch_x + int(swatch_w * 0.42), warm_bottom)),
            fill=SERIES_COLOR_PAIRS["zccache"]["warm"],
        )
        draw_fit(
            swatch_x + swatch_w + 12,
            legend_top + 2,
            "cold (back)  +  warm (front, overlays cold)",
            legend_font,
            "#c9d1d9",
            chart_w - swatch_w - 40,
        )
        # Violation swatch on the right side of the legend row
        violation_x = x0 + chart_w - inner_padding - 220
        draw.rectangle(
            box((violation_x, warm_top, violation_x + 30, warm_bottom)),
            fill=SERIES_COLOR_PAIRS["zccache"]["warm"],
            outline=WARM_VIOLATION_OUTLINE,
            width=2 * scale,
        )
        draw_fit(
            violation_x + 38,
            legend_top + 2,
            "warm > cold = cache regression",
            legend_font,
            WARM_VIOLATION_OUTLINE,
            220 - 40,
        )
        return y + legend_h

    def draw_scenario_block(y: int, combined_row: dict[str, Any], index: int) -> int:
        block_h = scenario_block_height()
        # Subtle alternating background per scenario
        fill = "#0f1620" if index % 2 == 0 else "#11202d"
        draw.rectangle(box((x0, y, x0 + chart_w, y + block_h)), fill=fill)

        # Per-scenario normalization
        scale_max, source = _normalization_reference(combined_row)
        violations = _warm_violations(combined_row)

        # Scenario heading
        heading = (
            f"{combined_row['compact_label']} - {combined_row['compact_scenario']}"
        )
        draw_fit(
            x0 + inner_padding,
            y + scenario_padding_top,
            heading,
            scenario_font,
            "#f0f6fc",
            chart_w - inner_padding * 2 - 260,
        )
        # Per-scenario scale note (right-aligned in the heading row)
        if source == "cold":
            scale_note = f"100% = cold max {scale_max:.3f}s"
        elif source == "warm":
            scale_note = f"100% = warm max {scale_max:.3f}s (no cold data)"
        else:
            scale_note = "no data"
        scale_note_px = _text_width(draw, scale_note, subtitle_font)
        draw.text(
            point(
                x0 + chart_w - inner_padding - scale_note_px // scale,
                y + scenario_padding_top + 8,
            ),
            scale_note,
            font=subtitle_font,
            fill="#8b949e",
        )

        bars_y = y + scenario_padding_top + scenario_label_h
        cold_bar_thickness = 28
        warm_bar_thickness = 16

        for series_index, series in enumerate(SERIES_ORDER):
            row_top = bars_y + series_index * bar_row_h
            cold_top = row_top + (bar_row_h - cold_bar_thickness) // 2
            cold_bottom = cold_top + cold_bar_thickness
            warm_top = row_top + (bar_row_h - warm_bar_thickness) // 2
            warm_bottom = warm_top + warm_bar_thickness

            cold_seconds = combined_row["cold"].get(series)
            warm_seconds = combined_row["warm"].get(series)
            cache_bytes = combined_row["cache_bytes"].get(series)
            pair = SERIES_COLOR_PAIRS[series]

            # Series label (use bare_label for the "bare" row)
            if series == "bare":
                series_label = str(combined_row.get("bare_label") or "bare").lower()
            else:
                series_label = series
            draw_fit(
                x0 + inner_padding,
                row_top + (bar_row_h - 19) // 2,
                series_label,
                series_font,
                pair["warm"],
                series_label_w - 8,
            )

            # Background track
            track_top = row_top + bar_row_h // 2 - 2
            draw.rectangle(
                box((bar_area_x0, track_top, bar_area_x1, track_top + 4)),
                fill="#21262d",
            )

            # Cold bar (back)
            if isinstance(cold_seconds, (int, float)) and cold_seconds > 0:
                fraction = max(0.0, min(1.0, cold_seconds / scale_max))
                bar_end = bar_area_x0 + max(3, int(round(bar_area_w * fraction)))
                draw.rectangle(
                    box((bar_area_x0, cold_top, bar_end, cold_bottom)),
                    fill=pair["cold"],
                )

            # Warm bar (front, overlay)
            if isinstance(warm_seconds, (int, float)) and warm_seconds > 0:
                fraction = max(0.0, min(1.0, warm_seconds / scale_max))
                bar_end = bar_area_x0 + max(3, int(round(bar_area_w * fraction)))
                if series in violations:
                    draw.rectangle(
                        box((bar_area_x0, warm_top, bar_end, warm_bottom)),
                        fill=pair["warm"],
                        outline=WARM_VIOLATION_OUTLINE,
                        width=2 * scale,
                    )
                else:
                    draw.rectangle(
                        box((bar_area_x0, warm_top, bar_end, warm_bottom)),
                        fill=pair["warm"],
                    )

            # Value labels: compact cold / warm / cache stack.
            value_x = bar_area_x1 + 12
            draw_fit(
                value_x,
                row_top + 4,
                f"cold {_format_seconds_label(cold_seconds)}",
                value_font,
                pair["cold"] if isinstance(cold_seconds, (int, float)) and cold_seconds > 0 else "#6e7681",
                value_label_w - 12,
            )
            warm_color = WARM_VIOLATION_OUTLINE if series in violations else (
                pair["warm"] if isinstance(warm_seconds, (int, float)) and warm_seconds > 0 else "#6e7681"
            )
            draw_fit(
                value_x,
                row_top + bar_row_h // 2 + 2,
                f"warm {_format_seconds_label(warm_seconds)}",
                value_font,
                warm_color,
                value_label_w - 12,
            )
            draw_fit(
                value_x,
                row_top + bar_row_h - 15,
                f"cache {_format_bytes(cache_bytes)}",
                small_font,
                "#8b949e",
                value_label_w - 12,
            )

        # Subtle separator under the block
        draw.line(
            box(
                (
                    x0 + inner_padding,
                    y + block_h - 1,
                    x0 + chart_w - inner_padding,
                    y + block_h - 1,
                )
            ),
            fill="#30363d",
            width=scale,
        )
        return y + block_h

    # Chart body -------------------------------------------------------------
    y = chart_top
    y = draw_legend(y)
    if not combined_rows:
        draw.rectangle(box((x0, y, x0 + chart_w, y + empty_section_h)), fill="#161b22")
        draw_fit(
            x0 + inner_padding,
            y + 28,
            "Benchmark data is not available yet.",
            scenario_font,
            "#f0f6fc",
            chart_w - inner_padding * 2,
        )
        y += empty_section_h
    else:
        for index, combined_row in enumerate(combined_rows):
            y = draw_scenario_block(y, combined_row, index)
            if index != len(combined_rows) - 1:
                y += scenario_gap

    # Footer -----------------------------------------------------------------
    draw.rectangle(
        box((margin, height - 36, width - margin, height - 34)), fill="#30363d"
    )
    footer = (
        "Artifacts: latest.json, benchmark-c.jpg, benchmark-cpp.jpg, "
        "benchmark-emscripten.jpg, benchmark-rust.jpg"
    )
    draw_fit(
        margin,
        height - 26,
        footer,
        small_font,
        "#8b949e",
        width - margin * 2,
    )

    path.parent.mkdir(parents=True, exist_ok=True)
    resampling = getattr(Image, "Resampling", Image).LANCZOS
    image = image.resize((width, height), resampling)
    image.save(path, format="JPEG", quality=90, optimize=True)


def write_outputs(payload: dict[str, Any], output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "latest.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    write_history_jsonl(payload, output_dir)
    (output_dir / "index.html").write_text(render_html(payload), encoding="utf-8")
    (output_dir / ".nojekyll").write_text("", encoding="utf-8")
    allowed_images = set(LANGUAGE_IMAGE_FILES.values())
    for image_path in output_dir.glob("benchmark*.jpg"):
        if image_path.name not in allowed_images:
            image_path.unlink()
    for language, image_file in LANGUAGE_IMAGE_FILES.items():
        render_language_jpg(payload, language, output_dir / image_file)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT_DIR)
    parser.add_argument(
        "--input-log",
        type=Path,
        help="Parse an existing perf log instead of running benchmarks.",
    )
    parser.add_argument(
        "--run-benchmarks",
        action="store_true",
        help="Run the ignored perf benchmark suite before generating artifacts.",
    )
    parser.add_argument("--log-path", type=Path, default=DEFAULT_OUTPUT_DIR / "benchmark.log")
    args = parser.parse_args(argv)

    if args.input_log:
        text = args.input_log.read_text(encoding="utf-8")
    elif args.run_benchmarks:
        text = run_benchmarks(args.log_path)
    else:
        parser.error("use --run-benchmarks or --input-log")

    results = parse_benchmark_log(text)
    if not results:
        raise SystemExit("no benchmark result rows found in benchmark output")
    payload = build_payload(results, collect_metadata())
    write_outputs(payload, args.output_dir)
    print(f"wrote benchmark stats to {args.output_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
