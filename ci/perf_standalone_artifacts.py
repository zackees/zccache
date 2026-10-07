"""Artifact validation and host-side publication for standalone campaigns."""

from __future__ import annotations

import json
import re
import tempfile
from pathlib import Path
from typing import Any


def validate_sample_summary(summary: dict[str, Any], expected_attempts: int) -> None:
    if summary.get("passed") is not True:
        raise ValueError("sample did not pass the strict perf guard")
    if summary.get("attempt_policy") != "all-required":
        raise ValueError("sample was not collected with the all-required policy")
    if summary.get("attempt_count") != expected_attempts:
        raise ValueError("sample attempt count is incomplete")
    if summary.get("command_failures"):
        raise ValueError("sample contains command failures")
    if summary.get("missing_requirements"):
        raise ValueError("sample contains missing benchmark rows")
    infrastructure = summary.get("infrastructure")
    if not isinstance(infrastructure, dict) or infrastructure.get("valid") is not True:
        raise ValueError("sample infrastructure is invalid")
    if infrastructure.get("invalid_reasons"):
        raise ValueError("sample contains infrastructure invalidity")
    if infrastructure.get("fallback_count") != 0:
        raise ValueError("sample contains fallback telemetry")
    telemetry = infrastructure.get("cache_telemetry")
    if not isinstance(telemetry, dict) or not telemetry.get("rows"):
        raise ValueError("sample contains no cache hit/miss telemetry")
    if telemetry.get("fallback_count") != 0:
        raise ValueError("sample cache telemetry contains a fallback")
    cache_byte_fields = (
        "bare_cache_bytes",
        "sccache_cache_bytes",
        "zccache_cache_bytes",
    )
    for row in telemetry["rows"]:
        if row.get("cache_phase") not in {"warm-hit-path", "cold-miss-path"}:
            raise ValueError("sample row contains an unknown cache phase")
        if row.get("cache_bytes_reported") is not True:
            raise ValueError("sample row does not report cache bytes")
        if any(not isinstance(row.get(field), int) for field in cache_byte_fields):
            raise ValueError("sample row contains invalid cache-byte telemetry")
    statuses = summary.get("statuses")
    if not isinstance(statuses, list) or not statuses:
        raise ValueError("sample contains no scenario statuses")
    for status in statuses:
        samples = status.get("samples")
        if not isinstance(samples, list) or len(samples) != expected_attempts:
            raise ValueError("scenario has an incomplete sample distribution")
        for sample in samples:
            if not sample.get("attempt_json") or not sample.get("raw_log"):
                raise ValueError("scenario sample is missing raw artifact references")


def artifact_paths(campaign_dir: Path, sample_dir: Path) -> dict[str, Any]:
    def relative(path: Path) -> str:
        return path.relative_to(campaign_dir).as_posix()

    return {
        "summary_json": relative(sample_dir / "perf-guard-summary.json"),
        "summary_markdown": relative(sample_dir / "perf-guard-summary.md"),
        "result": relative(sample_dir / "perf-guard-result.txt"),
        "resource_usage": relative(sample_dir / "resource-usage.txt"),
        "raw_logs": [relative(path) for path in sorted(sample_dir.glob("attempt-*.log"))],
        "attempt_json": [
            relative(path) for path in sorted(sample_dir.glob("attempt-*.json"))
        ],
    }


def _load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    # Docker can own the existing summary even though the invoking user
    # owns its parent directory. Publish a replacement without opening the
    # container's inode for writing. Keep the previous evidence on failure.
    serialized = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    with tempfile.TemporaryDirectory(prefix=f".{path.name}.", dir=path.parent) as directory:
        staged = Path(directory) / "payload.json"
        staged.write_text(serialized, encoding="utf-8")
        staged.replace(path)


def _peak_rss_bytes(path: Path) -> int | None:
    if not path.is_file():
        return None
    match = re.search(
        r"Maximum resident set size \(kbytes\):\s*(\d+)",
        path.read_text(encoding="utf-8", errors="replace"),
    )
    return int(match.group(1)) * 1024 if match else None


def _sample_cache_telemetry(sample_dir: Path) -> dict[str, Any]:
    rows = []
    for path in sorted(sample_dir.glob("attempt-*.json")):
        attempt = _load_json(path)
        for row in attempt.get("rows", []):
            if not isinstance(row, dict):
                continue
            mode = row.get("mode")
            cache_phase = {
                "warm": "warm-hit-path",
                "cold": "cold-miss-path",
            }.get(mode, "unknown")
            rows.append(
                {
                    "attempt": attempt.get("attempt"),
                    "benchmark": row.get("benchmark"),
                    "scenario": row.get("scenario"),
                    "cache_phase": cache_phase,
                    "cache_bytes_reported": row.get("cache_bytes_reported"),
                    "bare_cache_bytes": row.get("bare_cache_bytes"),
                    "sccache_cache_bytes": row.get("sccache_cache_bytes"),
                    "zccache_cache_bytes": row.get("zccache_cache_bytes"),
                }
            )
    return {
        "warm_hit_path_rows": sum(
            row["cache_phase"] == "warm-hit-path" for row in rows
        ),
        "cold_miss_path_rows": sum(
            row["cache_phase"] == "cold-miss-path" for row in rows
        ),
        "fallback_count": 0,
        "rows": rows,
    }


def _enrich_summary(
    path: Path,
    returncode: int,
    identity: dict[str, Any],
    command: list[str],
    telemetry: dict[str, Any],
) -> dict[str, Any]:
    summary = _load_json(path)
    reasons = []
    if returncode != 0:
        reasons.append(f"benchmark container exited with status {returncode}")
    if summary.get("command_failures"):
        reasons.append("perf guard reported command failures")
    if summary.get("missing_requirements"):
        reasons.append("perf guard reported missing rows")
    metadata = summary.setdefault("metadata", {})
    metadata.update(
        {
            "git_sha": identity["commit"],
            "git_ref": identity["ref"],
            "dirty": identity["dirty"],
            "image_digest": identity["image_digest"],
            "host_fingerprint": identity["host_fingerprint"],
            "docker_command": command,
        }
    )
    summary["infrastructure"] = {
        "valid": not reasons,
        "invalid_reasons": reasons,
        "fallback_count": 0,
        "cache_telemetry": telemetry,
        "fallback_contract": (
            "timed phase executes the prebuilt perf_bench_test directly; "
            "soldr is absent from the timed command path"
        ),
    }
    _write_json(path, summary)
    return summary


