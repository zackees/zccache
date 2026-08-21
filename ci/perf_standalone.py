"""Run the registered standalone performance suite in pinned Linux Docker."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from ci import benchmark_stats, perf_sample_monitor


REPO_ROOT = Path(__file__).resolve().parents[1]
DOCKERFILE = REPO_ROOT / "ci/docker/standalone-perf.Dockerfile"
IMAGE = "zccache-standalone-perf:1"
DEFAULT_RESULTS_ROOT = REPO_ROOT / ".perf-standalone/results"
DEFAULT_ATTEMPTS = 5
BUILD_VOLUMES = {
    "zccache-standalone-soldr-home": "/root/.soldr",
    "zccache-standalone-target": "/target",
}
FIXTURE_HASH_FIELDS = {
    "perf_bench_test": "benchmark_sha256",
    "zccache-ci": "zccache_ci_sha256",
}
TOOL_VERSION_MARKERS = {
    "rustc": "rustc 1.95.0",
    "clang": "clang version 14.",
    "sccache": "sccache 0.10.0",
    "emscripten": "3.1.74",
    "soldr": "0.8.16",
}
COMPETING_CONTAINERS = ("perf", "docker-build-soldr")
COMPETING_PROCESSES = {
    "cargo",
    "clang",
    "clang++",
    "em++",
    "emcc",
    "perf_bench_test",
    "rustc",
    "sccache",
}
BUSY_CPU_PERCENT = 5.0
HOST_PROCESS_ENUMERATION_TIMEOUT_SECONDS = 30
RECIPE_FILES = (
    DOCKERFILE,
    REPO_ROOT / "ci/docker/standalone_perf_entrypoint.sh",
    REPO_ROOT / "rust-toolchain.toml",
)


def campaign_inventory() -> list[tuple[str, str]]:
    return [
        (language, test_name)
        for language in benchmark_stats.LANGUAGES
        for test_name in benchmark_stats.BENCHMARK_TESTS_BY_LANGUAGE[language]
    ]


def recipe_sha256() -> str:
    digest = hashlib.sha256()
    for path in RECIPE_FILES:
        digest.update(path.relative_to(REPO_ROOT).as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def docker_build_command() -> list[str]:
    return [
        "docker",
        "build",
        "--build-arg",
        f"CAMPAIGN_RECIPE_SHA={recipe_sha256()}",
        "--file",
        str(DOCKERFILE),
        "--tag",
        IMAGE,
        str(REPO_ROOT),
    ]


def default_campaign_dir(
    commit: str,
    language: str | None,
    test_name: str | None,
    attempts: int,
) -> Path:
    selection = "full" if language is None else f"{language}-{test_name or 'all'}"
    slug = re.sub(r"[^a-z0-9]+", "-", selection.lower()).strip("-")
    return DEFAULT_RESULTS_ROOT / f"{commit[:12]}-{slug}-a{attempts}"


def _mount(source: str | Path, destination: str, *, kind: str, readonly: bool) -> str:
    value = f"type={kind},src={source},dst={destination}"
    return value + (",readonly" if readonly else "")


def docker_run_command(
    *,
    repo_root: Path,
    results_dir: Path,
    container_name: str,
    entrypoint_args: list[str],
    artifacts_dir: Path | None = None,
    artifacts_read_only: bool = True,
    results_read_only: bool = False,
) -> list[str]:
    artifacts_dir = artifacts_dir or results_dir / "fixture"
    command = ["docker", "run", "--rm", "--name", container_name]
    command.extend(
        [
            "--mount",
            _mount(repo_root.resolve(), "/src", kind="bind", readonly=True),
            "--mount",
            _mount(
                results_dir.resolve(),
                "/results",
                kind="bind",
                readonly=results_read_only,
            ),
            "--mount",
            _mount(
                artifacts_dir.resolve(),
                "/artifacts",
                kind="bind",
                readonly=artifacts_read_only,
            ),
        ]
    )
    for volume, destination in BUILD_VOLUMES.items():
        command.extend(
            [
                "--mount",
                _mount(
                    volume,
                    destination,
                    kind="volume",
                    readonly=False,
                ),
            ]
        )
    command.extend([IMAGE, *entrypoint_args])
    return command


def validate_tool_versions(versions: dict[str, str]) -> None:
    for tool, marker in TOOL_VERSION_MARKERS.items():
        actual = versions.get(tool)
        if not isinstance(actual, str) or marker not in actual:
            raise ValueError(
                f"{tool} version mismatch: expected marker {marker!r}, got {actual!r}"
            )


def validate_resume_identity(
    existing: dict[str, Any], current: dict[str, Any]
) -> None:
    for field in (
        "commit",
        "ref",
        "image_digest",
        "host_fingerprint",
        "inventory",
        "attempts",
        "fixture",
    ):
        if existing.get(field) != current.get(field):
            raise ValueError(
                f"resume identity mismatch for {field}: "
                f"{existing.get(field)!r} != {current.get(field)!r}"
            )


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


def busy_reasons(
    containers: list[tuple[str, float]], process_names: list[str]
) -> list[str]:
    reasons = []
    for name, cpu_percent in containers:
        lowered = name.lower()
        if any(marker in lowered for marker in COMPETING_CONTAINERS) and cpu_percent >= BUSY_CPU_PERCENT:
            reasons.append(f"container {name} is using {cpu_percent:.2f}% CPU")
    for name in sorted(set(process_names)):
        normalized = Path(name).stem.lower()
        if normalized in COMPETING_PROCESSES:
            reasons.append(f"host process {name} is active")
    return reasons


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


def _quarantine_contaminated_sample(sample_dir: Path) -> Path:
    destination = sample_dir.with_name(
        f"{sample_dir.name}.contaminated-{time.time_ns()}"
    )
    sample_dir.rename(destination)
    return destination


def _reject_contaminated_sample(sample_dir: Path, contamination: list[str]) -> None:
    _write_json(
        sample_dir / "infrastructure-invalid.json",
        {
            "valid": False,
            "invalid_reasons": [
                f"timed sample overlapped {reason}" for reason in contamination
            ],
        },
    )
    quarantine = _quarantine_contaminated_sample(sample_dir)
    raise RuntimeError(
        "timed sample was contaminated; evidence retained at "
        f"{quarantine}: " + "; ".join(contamination)
    )


def validate_completed_results(
    campaign_dir: Path, campaign: dict[str, Any], expected_attempts: int
) -> None:
    for item in campaign.get("results", []):
        artifacts = item.get("artifacts")
        if not isinstance(artifacts, dict):
            raise ValueError("completed campaign result has no artifact index")
        raw_logs = artifacts.get("raw_logs")
        attempt_json = artifacts.get("attempt_json")
        if not isinstance(raw_logs, list) or not isinstance(attempt_json, list):
            raise ValueError("completed campaign result has invalid sample artifacts")
        if len(raw_logs) != expected_attempts or len(attempt_json) != expected_attempts:
            raise ValueError("completed campaign result has incomplete sample artifacts")
        paths = [
            artifacts.get("summary_json"),
            artifacts.get("summary_markdown"),
            artifacts.get("result"),
            artifacts.get("resource_usage"),
            *raw_logs,
            *attempt_json,
        ]
        if any(not isinstance(path, str) for path in paths):
            raise ValueError("completed campaign result has an invalid artifact path")
        missing = [path for path in paths if not (campaign_dir / path).is_file()]
        if missing:
            raise ValueError(
                "completed campaign result is missing artifacts: " + ", ".join(missing)
            )
        validate_sample_summary(
            _load_json(campaign_dir / artifacts["summary_json"]), expected_attempts
        )


def _run(
    command: list[str],
    *,
    capture: bool = False,
    check: bool = True,
    timeout_seconds: float | None = None,
) -> subprocess.CompletedProcess[str]:
    print("+ " + subprocess.list2cmdline(command), flush=True)
    return subprocess.run(
        command,
        cwd=REPO_ROOT,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=capture,
        check=check,
        timeout=timeout_seconds,
    )


def _run_sample_command(
    command: list[str], container_name: str
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    print("+ " + subprocess.list2cmdline(command), flush=True)
    process = subprocess.Popen(command, cwd=REPO_ROOT, text=True, encoding="utf-8")
    return_code, contamination = perf_sample_monitor.monitor_sample_process(
        process,
        container_name,
        lambda finished: _sample_process_names(container_name, finished),
        _container_stats,
        busy_reasons,
    )
    return subprocess.CompletedProcess(command, return_code), contamination


def _git_output(*args: str) -> str:
    return _run(["git", *args], capture=True).stdout.strip()


def _ensure_clean_checkout() -> str:
    if _git_output("status", "--porcelain"):
        raise RuntimeError("standalone campaign requires a clean committed checkout")
    return _git_output("rev-parse", "HEAD")


def _build_image(rebuild: bool) -> None:
    inspected = subprocess.run(
        [
            "docker",
            "image",
            "inspect",
            "--format",
            '{{index .Config.Labels "org.zccache.campaign.recipe"}}',
            IMAGE,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    current_recipe = inspected.stdout.strip() if inspected.returncode == 0 else None
    if rebuild or current_recipe != recipe_sha256():
        _run(docker_build_command())


def _ensure_volumes() -> None:
    for volume in BUILD_VOLUMES:
        _run(["docker", "volume", "create", volume], capture=True)


def _image_digest() -> str:
    return _run(
        ["docker", "image", "inspect", "--format", "{{.Id}}", IMAGE],
        capture=True,
    ).stdout.strip()


def _container_stats() -> list[tuple[str, float]]:
    try:
        output = _run(
            [
                "docker",
                "stats",
                "--no-stream",
                "--format",
                "{{.Name}}|{{.CPUPerc}}",
            ],
            capture=True,
            timeout_seconds=15,
        ).stdout
    except subprocess.TimeoutExpired as error:
        raise RuntimeError("Docker container activity probe timed out") from error
    parsed = []
    for line in output.splitlines():
        name, separator, percent = line.partition("|")
        if separator:
            try:
                parsed.append((name, float(percent.rstrip("%"))))
            except ValueError:
                continue
    return parsed


def _process_names() -> list[str]:
    if os.name == "nt":
        try:
            result = subprocess.run(
                ["tasklist", "/fo", "csv", "/nh"],
                capture_output=True,
                text=True,
                check=False,
                timeout=HOST_PROCESS_ENUMERATION_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("host process enumeration timed out") from error
        if result.returncode != 0 or not result.stdout.strip():
            raise RuntimeError("host process enumeration failed")
        output = result.stdout
        return [row[0] for row in csv.reader(output.splitlines()) if row]
    output = subprocess.run(
        ["ps", "-eo", "comm="],
        capture_output=True,
        text=True,
        check=False,
    ).stdout
    return [line.strip() for line in output.splitlines() if line.strip()]


def _sample_container_process_ids(container_name: str) -> set[int]:
    top = subprocess.run(
        ["docker", "top", container_name, "-eo", "pid"],
        capture_output=True,
        text=True,
        check=False,
        timeout=HOST_PROCESS_ENUMERATION_TIMEOUT_SECONDS,
    )
    if top.returncode != 0:
        raise RuntimeError("sample container process enumeration failed")
    return {
        int(line.strip())
        for line in top.stdout.splitlines()
        if line.strip().isdigit()
    }


def _linux_sample_process_names(container_name: str) -> list[str]:
    container_pids_before = _sample_container_process_ids(container_name)
    host = subprocess.run(
        ["ps", "-eo", "pid=,comm="],
        capture_output=True,
        text=True,
        check=False,
        timeout=HOST_PROCESS_ENUMERATION_TIMEOUT_SECONDS,
    )
    if host.returncode != 0:
        raise RuntimeError("host process enumeration failed")
    processes = []
    for line in host.stdout.splitlines():
        fields = line.strip().split(maxsplit=1)
        if len(fields) == 2 and fields[0].isdigit():
            processes.append((int(fields[0]), fields[1]))
    container_pids_after = _sample_container_process_ids(container_name)
    return perf_sample_monitor.process_names_outside_container(
        processes, container_pids_before, container_pids_after
    )


def _sample_process_names(container_name: str, finished: bool) -> list[str]:
    if os.name == "nt" or finished:
        return _active_process_names()
    return _linux_sample_process_names(container_name)


def active_windows_process_names(
    first: dict[int, tuple[str, float]],
    second: dict[int, tuple[str, float]],
    minimum_cpu_seconds: float = 0.001,
) -> list[str]:
    cargo_present = any(
        Path(name).stem.lower() == "cargo" for name, _ in second.values()
    )
    active = []
    for pid, (name, cpu_seconds) in second.items():
        normalized = Path(name).stem.lower()
        if normalized not in COMPETING_PROCESSES:
            continue
        if normalized != "rustc":
            active.append(name)
            continue
        previous = first.get(pid)
        cpu_progress = previous is None or (
            cpu_seconds - previous[1] >= minimum_cpu_seconds
        )
        if cargo_present or cpu_progress:
            active.append(name)
    return active


def _windows_process_snapshot() -> dict[int, tuple[str, float]] | None:
    import ctypes
    from ctypes import wintypes

    try:
        result = subprocess.run(
            ["tasklist", "/fo", "csv", "/nh"],
            capture_output=True,
            text=True,
            check=False,
            timeout=HOST_PROCESS_ENUMERATION_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return None
    if result.returncode != 0 or not result.stdout.strip():
        return None

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.GetProcessTimes.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(wintypes.FILETIME),
        ctypes.POINTER(wintypes.FILETIME),
        ctypes.POINTER(wintypes.FILETIME),
        ctypes.POINTER(wintypes.FILETIME),
    ]
    kernel32.GetProcessTimes.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL

    snapshot = {}
    for row in csv.reader(result.stdout.splitlines()):
        if len(row) < 2 or Path(row[0]).stem.lower() not in COMPETING_PROCESSES:
            continue
        try:
            pid = int(row[1])
        except ValueError:
            continue
        handle = kernel32.OpenProcess(0x1000, False, pid)
        if not handle:
            continue
        try:
            creation = wintypes.FILETIME()
            exit_time = wintypes.FILETIME()
            kernel = wintypes.FILETIME()
            user = wintypes.FILETIME()
            if not kernel32.GetProcessTimes(
                handle,
                ctypes.byref(creation),
                ctypes.byref(exit_time),
                ctypes.byref(kernel),
                ctypes.byref(user),
            ):
                continue
            kernel_ticks = kernel.dwLowDateTime | (kernel.dwHighDateTime << 32)
            user_ticks = user.dwLowDateTime | (user.dwHighDateTime << 32)
            snapshot[pid] = (row[0], (kernel_ticks + user_ticks) / 10_000_000)
        finally:
            kernel32.CloseHandle(handle)
    return snapshot


def _active_process_names() -> list[str]:
    if os.name != "nt":
        return _process_names()
    names = _process_names()
    competing = [
        name
        for name in names
        if Path(name).stem.lower() in COMPETING_PROCESSES
    ]
    if not any(Path(name).stem.lower() == "rustc" for name in competing):
        return competing
    if any(Path(name).stem.lower() != "rustc" for name in competing):
        return competing
    first = _windows_process_snapshot()
    if first is None:
        return competing
    started = time.monotonic()
    time.sleep(1.0)
    second = _windows_process_snapshot()
    if second is None:
        return competing
    elapsed = time.monotonic() - started
    minimum_cpu_seconds = (
        elapsed * max(os.cpu_count() or 1, 1) * BUSY_CPU_PERCENT / 100.0
    )
    return active_windows_process_names(first, second, minimum_cpu_seconds)


def _require_quiet_host(samples: int = 3, delay_seconds: float = 2.0) -> None:
    for sample in range(samples):
        process_reasons = busy_reasons([], _active_process_names())
        if process_reasons:
            raise RuntimeError("host is busy: " + "; ".join(process_reasons))
        reasons = busy_reasons(_container_stats(), [])
        if reasons:
            raise RuntimeError("host is busy: " + "; ".join(reasons))
        if sample + 1 < samples:
            time.sleep(delay_seconds)


def stable_docker_identity(info: dict[str, Any]) -> dict[str, Any]:
    fields = (
        "ID",
        "Name",
        "ServerVersion",
        "Driver",
        "OperatingSystem",
        "OSType",
        "Architecture",
        "NCPU",
        "MemTotal",
        "DockerRootDir",
        "KernelVersion",
    )
    return {field: info.get(field) for field in fields}


def _host_identity() -> dict[str, Any]:
    docker = _run(
        ["docker", "info", "--format", "{{json .}}"], capture=True
    ).stdout.strip()
    payload = {
        "node": platform.node(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "cpu_count": os.cpu_count(),
        "docker": stable_docker_identity(json.loads(docker)),
    }
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    payload["fingerprint"] = hashlib.sha256(canonical.encode()).hexdigest()
    return payload


def _benchmark_build_command(campaign_dir: Path) -> list[str]:
    return docker_run_command(
        repo_root=REPO_ROOT,
        results_dir=campaign_dir,
        container_name="zccache-standalone-build",
        entrypoint_args=["build"],
        artifacts_dir=campaign_dir / "fixture",
        artifacts_read_only=False,
    )


def _build_benchmark(campaign_dir: Path) -> list[str]:
    command = _benchmark_build_command(campaign_dir)
    _run(command)
    return command


def _tool_versions(
    campaign_dir: Path,
) -> tuple[dict[str, str], dict[str, str], list[str]]:
    command = docker_run_command(
        repo_root=REPO_ROOT,
        results_dir=campaign_dir,
        container_name="zccache-standalone-verify",
        entrypoint_args=["verify"],
        artifacts_dir=campaign_dir / "fixture",
        results_read_only=True,
    )
    versions = json.loads(_run(command, capture=True).stdout)
    validate_tool_versions(versions)
    fixture_hashes = {}
    for artifact, field in FIXTURE_HASH_FIELDS.items():
        sha256 = versions.pop(field, "")
        if not isinstance(sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", sha256):
            raise ValueError(f"{artifact} fixture SHA-256 is missing or invalid")
        fixture_hashes[artifact] = sha256
    return versions, fixture_hashes, command


def _load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


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


def _run_sample(
    campaign_dir: Path,
    language: str,
    test_name: str,
    attempts: int,
    identity: dict[str, Any],
) -> dict[str, Any]:
    sample_dir = campaign_dir / language.replace("+", "p") / test_name
    sample_dir.mkdir(parents=True, exist_ok=True)
    container_name = f"zccache-standalone-{language.replace('+', 'p')}-{test_name}"[:63]
    command = docker_run_command(
        repo_root=REPO_ROOT,
        results_dir=sample_dir,
        container_name=container_name,
        entrypoint_args=["run", language, test_name, str(attempts)],
        artifacts_dir=campaign_dir / "fixture",
    )
    result, contamination = _run_sample_command(command, container_name)
    summary_path = sample_dir / "perf-guard-summary.json"
    if contamination and not summary_path.is_file():
        _reject_contaminated_sample(sample_dir, contamination)
    if not summary_path.is_file():
        raise RuntimeError(
            f"benchmark did not produce {summary_path} (exit={result.returncode})"
        )
    telemetry = _sample_cache_telemetry(sample_dir)
    summary = _enrich_summary(
        summary_path, result.returncode, identity, command, telemetry
    )
    if contamination:
        infrastructure = summary["infrastructure"]
        infrastructure["valid"] = False
        infrastructure["invalid_reasons"].extend(
            f"timed sample overlapped {reason}" for reason in contamination
        )
        _write_json(summary_path, summary)
        _reject_contaminated_sample(sample_dir, contamination)
    validate_sample_summary(summary, attempts)
    return {
        "language": language,
        "test": test_name,
        "passed": True,
        "peak_rss_bytes": _peak_rss_bytes(sample_dir / "resource-usage.txt"),
        "command": command,
        "cache_telemetry": telemetry,
        "artifacts": artifact_paths(campaign_dir, sample_dir),
    }


def _render_markdown(campaign: dict[str, Any]) -> str:
    lines = [
        "# Standalone Linux Docker performance campaign",
        "",
        f"- Commit: `{campaign['identity']['commit']}`",
        f"- Image: `{campaign['identity']['image_digest']}`",
        f"- Attempts per test: {campaign['identity']['attempts']}",
        f"- Host: `{campaign['identity']['host_fingerprint']}`",
        "",
        "| Language | Test | Status | Peak RSS | Summary | Raw logs |",
        "|---|---|---|---:|---|---|",
    ]
    for item in campaign["results"]:
        rss = item.get("peak_rss_bytes")
        rss_text = "n/a" if rss is None else f"{rss / 1024 / 1024:.1f} MiB"
        artifacts = item["artifacts"]
        log_links = ", ".join(
            f"[#{index}]({path})"
            for index, path in enumerate(artifacts["raw_logs"], start=1)
        )
        lines.append(
            f"| {item['language']} | `{item['test']}` | PASS | {rss_text} | "
            f"[JSON]({artifacts['summary_json']}) | {log_links} |"
        )
    return "\n".join(lines) + "\n"


def _selected_inventory(language: str | None, test_name: str | None) -> list[tuple[str, str]]:
    inventory = campaign_inventory()
    if language:
        inventory = [item for item in inventory if item[0] == language]
    if test_name:
        inventory = [item for item in inventory if item[1] == test_name]
    if not inventory:
        raise ValueError("no registered benchmark matches the requested filters")
    return inventory


def run_campaign(args: argparse.Namespace) -> Path:
    commit = _ensure_clean_checkout()
    inventory = _selected_inventory(args.language, args.test)
    campaign_dir = (
        args.output_dir
        or default_campaign_dir(commit, args.language, args.test, args.attempts)
    ).resolve()
    identity_path = campaign_dir / "campaign-identity.json"
    if args.resume:
        if not identity_path.is_file():
            raise RuntimeError(
                f"cannot resume campaign without {identity_path}"
            )
    else:
        campaign_dir.mkdir(parents=True, exist_ok=True)
        if identity_path.exists():
            raise RuntimeError(
                f"campaign already exists: {campaign_dir}; pass --resume"
            )
        (campaign_dir / "fixture").mkdir(parents=True, exist_ok=True)

    _build_image(args.rebuild_image)
    _ensure_volumes()
    benchmark_build_command = (
        _benchmark_build_command(campaign_dir)
        if args.resume
        else _build_benchmark(campaign_dir)
    )
    versions, fixture_hashes, verify_command = _tool_versions(campaign_dir)
    host = _host_identity()
    identity = {
        "commit": commit,
        "ref": _git_output("rev-parse", "--abbrev-ref", "HEAD"),
        "dirty": False,
        "image_digest": _image_digest(),
        "host_fingerprint": host["fingerprint"],
        "inventory": [list(item) for item in inventory],
        "attempts": args.attempts,
        "fixture": {
            "artifacts": fixture_hashes,
        },
    }
    if args.resume:
        validate_resume_identity(_load_json(identity_path), identity)
    else:
        _write_json(identity_path, identity)

    campaign_path = campaign_dir / "campaign.json"
    campaign = _load_json(campaign_path) if args.resume and campaign_path.exists() else {
        "schema_version": 1,
        "identity": identity,
        "host": host,
        "tool_versions": versions,
        "commands": {
            "image_build": docker_build_command(),
            "benchmark_build": benchmark_build_command,
            "verify": verify_command,
        },
        "results": [],
    }
    if args.resume:
        validate_completed_results(campaign_dir, campaign, args.attempts)
    completed = {(item["language"], item["test"]) for item in campaign["results"]}
    for language, test_name in inventory:
        if (language, test_name) in completed:
            continue
        _require_quiet_host()
        item = _run_sample(
            campaign_dir, language, test_name, args.attempts, identity
        )
        campaign["results"].append(item)
        _write_json(campaign_path, campaign)
        (campaign_dir / "README.md").write_text(
            _render_markdown(campaign), encoding="utf-8"
        )
    return campaign_dir


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--language", choices=benchmark_stats.LANGUAGES)
    parser.add_argument("--test")
    parser.add_argument("--attempts", type=int, default=DEFAULT_ATTEMPTS)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--rebuild-image", action="store_true")
    args = parser.parse_args()
    if args.attempts < 1:
        parser.error("--attempts must be at least 1")
    if args.test and not args.language:
        parser.error("--test requires --language")
    try:
        output = run_campaign(args)
    except (RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"campaign complete: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
