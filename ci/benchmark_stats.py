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
            scenario = cells[0]
            mode = "warm" if "warm" in scenario.lower() else "cold"
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
                "zccache_vs_sccache_ratio": _ratio(sccache_seconds, zccache_seconds),
                "zccache_vs_bare_ratio": _ratio(bare_seconds, zccache_seconds),
                "vs_sccache_text": cells[4],
                "vs_bare_text": cells[5],
            }
            results.append(result)

    return results


def _format_seconds(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.3f}s"


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
        "schema_version": 1,
        "metadata": metadata,
        "summary": build_summary(results),
        "results": results,
    }


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
            "<tr class=\"group\"><th colspan=\"6\">" + escape(group) + "</th></tr>"
        )
        for row in rows:
            rows_html.append(
                "<tr>"
                f"<td>{escape(row['scenario'])}</td>"
                f"<td>{escape(_format_seconds(row['bare_seconds']))}</td>"
                f"<td>{escape(_format_seconds(row['sccache_seconds']))}</td>"
                f"<td class=\"strong\">{escape(_format_seconds(row['zccache_seconds']))}</td>"
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
        Raw machine-readable data: <a href="latest.json">latest.json</a>.
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
            "vs_sccache": _format_ratio(row["zccache_vs_sccache_ratio"]),
            "vs_bare": _format_ratio(row["zccache_vs_bare_ratio"]),
            "vs_sccache_percent": _format_percent_delta(row["zccache_vs_sccache_ratio"]),
            "vs_bare_percent": _format_percent_delta(row["zccache_vs_bare_ratio"]),
            "zccache_vs_sccache_ratio": row["zccache_vs_sccache_ratio"],
            "zccache_vs_bare_ratio": row["zccache_vs_bare_ratio"],
        }
        for row in results
    ]


def _section_rows(rows: list[dict[str, Any]], mode: str) -> list[dict[str, Any]]:
    return [row for row in rows if row["mode"] == mode]


def render_language_jpg(payload: dict[str, Any], language: str, path: Path) -> None:
    try:
        from PIL import Image, ImageDraw
    except ImportError as exc:
        raise SystemExit(
            "Pillow is required to write benchmark JPGs. Install it with "
            "`uv run --with pillow` or `python -m pip install Pillow`."
        ) from exc

    rows = build_image_rows(group_results_by_language(payload["results"])[language])
    title = f"zccache {LANGUAGE_LABELS[language]} benchmarks"
    width = 900
    margin = 20
    row_h = 76
    section_h = 28
    header_h = 34
    table_y = 106
    footer_h = 44
    section_count = 2
    rendered_row_count = max(1, len(rows))
    height = max(
        390,
        table_y + header_h + section_h * section_count + row_h * rendered_row_count + footer_h,
    )
    scale = 4
    image = Image.new("RGB", (width * scale, height * scale), "#0d1117")
    draw = ImageDraw.Draw(image)
    title_font = _font(26 * scale, bold=True)
    subtitle_font = _font(11 * scale)
    header_font = _font(12 * scale, bold=True)
    section_font = _font(14 * scale, bold=True)
    row_font = _font(12 * scale)
    row_bold_font = _font(12 * scale, bold=True)
    scenario_font = _font(13 * scale, bold=True)
    delta_font = _font(12 * scale)
    delta_bold_font = _font(14 * scale, bold=True)
    small_font = _font(10 * scale)

    def box(values: tuple[int, int, int, int]) -> tuple[int, int, int, int]:
        return tuple(value * scale for value in values)

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

    def draw_delta(x: int, y: int, value: str, ratio: float | None, max_width: int) -> None:
        color = ratio_color(ratio)
        if value == "n/a":
            draw_fit(x, y + 19, value, delta_bold_font, color, max_width)
            return
        amount, _, direction = value.partition(" ")
        draw_fit(x, y + 12, amount, delta_bold_font, color, max_width)
        if direction:
            draw_fit(x, y + 38, direction, delta_font, color, max_width)

    draw.rectangle(box((0, 0, width, height)), fill="#0d1117")
    draw.rectangle(box((0, 0, width, 84)), fill="#161b22")
    draw.text(point(margin, 20), title, font=title_font, fill="#f0f6fc")
    metadata = payload["metadata"]
    sha = (metadata.get("git_sha") or "n/a")[:12]
    runner = metadata.get("runner", {}).get("platform") or "n/a"
    metadata_line = (
        f"Generated {metadata['generated_at']} | ref {metadata.get('git_ref') or 'n/a'} | "
        f"sha {sha} | runner {runner}"
    )
    draw.text(
        point(margin, 60),
        _truncate_to_width(draw, metadata_line, subtitle_font, (width - margin * 2) * scale),
        font=subtitle_font,
        fill="#8b949e",
    )

    x0, y0 = margin, table_y
    table_w = width - margin * 2
    headers = ["Scenario", "Times", "vs sccache", "vs bare"]
    widths = [250, 280, 160, 160]
    padding_x = 10

    draw.rounded_rectangle(
        box((x0, y0, x0 + table_w, y0 + header_h)),
        radius=8 * scale,
        fill="#21262d",
        outline="#30363d",
        width=scale,
    )
    x = x0 + padding_x
    for header, col_w in zip(headers, widths):
        draw.text(point(x, y0 + 10), header, font=header_font, fill="#c9d1d9")
        x += col_w

    y = y0 + header_h
    if not rows:
        draw.rectangle(box((x0, y, x0 + table_w, y + row_h)), fill="#161b22")
        draw_fit(
            x0 + padding_x,
            y + 28,
            "Benchmark data is not available yet.",
            row_font,
            "#f0f6fc",
            table_w - padding_x * 2,
        )
        y += row_h
    else:
        section_styles = {
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
        for mode in ("cold", "warm"):
            style = section_styles[mode]
            draw.rectangle(box((x0, y, x0 + table_w, y + section_h)), fill=style["section"])
            draw.text(
                point(x0 + padding_x, y + 7),
                style["title"],
                font=section_font,
                fill=style["accent"],
            )
            y += section_h
            for index, row in enumerate(_section_rows(rows, mode)):
                fill = style["row"] if index % 2 == 0 else style["row_alt"]
                draw.rectangle(box((x0, y, x0 + table_w, y + row_h)), fill=fill)
                draw.line(
                    box((x0, y + row_h, x0 + table_w, y + row_h)),
                    fill="#30363d",
                    width=scale,
                )
                x = x0 + padding_x
                draw_fit(
                    x,
                    y + 14,
                    row["compact_label"],
                    row_font,
                    "#8b949e",
                    widths[0] - 16,
                )
                draw_fit(
                    x,
                    y + 38,
                    row["compact_scenario"],
                    scenario_font,
                    "#f0f6fc",
                    widths[0] - 16,
                )

                x += widths[0]
                time_lines = [
                    ("bare", row["bare"], row_font, "#c9d1d9"),
                    ("sccache", row["sccache"], row_font, "#c9d1d9"),
                    ("zccache", row["zccache"], row_bold_font, "#f0f6fc"),
                ]
                for line_index, (label, value, font, color) in enumerate(time_lines):
                    draw_fit(
                        x,
                        y + 8 + line_index * 21,
                        f"{label}: {value}",
                        font,
                        color,
                        widths[1] - 18,
                    )

                x += widths[1]
                draw_delta(
                    x,
                    y,
                    row["vs_sccache_percent"],
                    row["zccache_vs_sccache_ratio"],
                    widths[2] - 18,
                )
                x += widths[2]
                draw_delta(
                    x,
                    y,
                    row["vs_bare_percent"],
                    row["zccache_vs_bare_ratio"],
                    widths[3] - 18,
                )
                y += row_h

    draw.rectangle(box((margin, height - 34, width - margin, height - 32)), fill="#30363d")
    footer = "Artifacts: latest.json, benchmark-c.jpg, benchmark-cpp.jpg, benchmark-rust.jpg"
    draw.text(
        point(margin, height - 24),
        _truncate_to_width(draw, footer, small_font, (width - margin * 2) * scale),
        font=small_font,
        fill="#8b949e",
    )

    path.parent.mkdir(parents=True, exist_ok=True)
    resampling = getattr(Image, "Resampling", Image).LANCZOS
    image = image.resize((width, height), resampling)
    image.save(path, format="JPEG", quality=90, optimize=True)


def write_outputs(payload: dict[str, Any], output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "latest.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
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
