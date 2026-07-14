#!/usr/bin/env python3
"""Measure representative warm incremental zccache rebuilds.

The benchmark uses ``soldr cargo`` and touches source mtimes without changing
file contents. Results are JSON so crate-boundary changes can be compared using
the same edit surfaces before and after a split.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import platform
import statistics
import subprocess
import tempfile
import threading
import time
from dataclasses import asdict, dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
EDIT_SURFACES = {
    "compile": Path("crates/zccache-daemon-core/src/daemon/server/handle_compile/pipeline/mod.rs"),
    "link": Path("crates/zccache-daemon-core/src/daemon/server/handle_link.rs"),
    "exec": Path("crates/zccache-daemon-core/src/daemon/server/handle_exec.rs"),
    "connection": Path("crates/zccache-daemon-core/src/daemon/server/connection.rs"),
    "shared": Path("crates/zccache-daemon-core/src/daemon/server/state.rs"),
}


@dataclass(frozen=True)
class BuildSample:
    surface: str
    sample: int
    elapsed_ns: int
    peak_rss_bytes: int | None
    rebuilt_packages: list[str]
    touched_path: str


def package_name(package_id: str, manifest_path: str | None = None) -> str:
    """Return a stable package name from Cargo's package_id string."""
    tail = package_id.rsplit("#", 1)[-1]
    candidate = tail.split("@", 1)[0]
    if candidate and not candidate[0].isdigit():
        return candidate
    if manifest_path:
        return Path(manifest_path).parent.name
    return candidate


def parse_rebuilt_packages(output: str) -> list[str]:
    rebuilt: set[str] = set()
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact" or message.get("fresh", False):
            continue
        package_id = message.get("package_id")
        if isinstance(package_id, str):
            manifest_path = message.get("manifest_path")
            rebuilt.add(
                package_name(
                    package_id,
                    manifest_path if isinstance(manifest_path, str) else None,
                )
            )
    return sorted(rebuilt)


def summarize(samples: list[BuildSample]) -> dict[str, object]:
    elapsed = [sample.elapsed_ns for sample in samples]
    rss = [sample.peak_rss_bytes for sample in samples if sample.peak_rss_bytes is not None]
    median = int(statistics.median(elapsed))
    return {
        "samples": len(samples),
        "min_ns": min(elapsed),
        "median_ns": median,
        "max_ns": max(elapsed),
        "mad_ns": int(statistics.median(abs(value - median) for value in elapsed)),
        "peak_rss_bytes": max(rss) if rss else None,
        "rebuilt_packages": sorted({pkg for sample in samples for pkg in sample.rebuilt_packages}),
    }


def _windows_process_rss() -> dict[int, tuple[int, int]]:
    from ctypes import wintypes

    th32cs_snapp_process = 0x00000002
    process_query_limited_information = 0x1000
    process_vm_read = 0x0010
    invalid_handle_value = ctypes.c_void_p(-1).value

    class ProcessEntry32W(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD),
            ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", wintypes.DWORD),
            ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD),
            ("pcPriClassBase", wintypes.LONG),
            ("dwFlags", wintypes.DWORD),
            ("szExeFile", wintypes.WCHAR * 260),
        ]

    class ProcessMemoryCounters(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel32.Process32FirstW.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(ProcessEntry32W),
    ]
    kernel32.Process32FirstW.restype = wintypes.BOOL
    kernel32.Process32NextW.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(ProcessEntry32W),
    ]
    kernel32.Process32NextW.restype = wintypes.BOOL
    kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL
    psapi.GetProcessMemoryInfo.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(ProcessMemoryCounters),
        wintypes.DWORD,
    ]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    snapshot = kernel32.CreateToolhelp32Snapshot(th32cs_snapp_process, 0)
    if snapshot == invalid_handle_value:
        return {}
    rows: dict[int, tuple[int, int]] = {}
    try:
        entry = ProcessEntry32W()
        entry.dwSize = ctypes.sizeof(entry)
        ok = kernel32.Process32FirstW(snapshot, ctypes.byref(entry))
        while ok:
            pid = int(entry.th32ProcessID)
            handle = kernel32.OpenProcess(process_query_limited_information | process_vm_read, False, pid)
            rss = 0
            if handle:
                counters = ProcessMemoryCounters()
                counters.cb = ctypes.sizeof(counters)
                if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                    rss = int(counters.WorkingSetSize)
                kernel32.CloseHandle(handle)
            rows[pid] = (int(entry.th32ParentProcessID), rss)
            ok = kernel32.Process32NextW(snapshot, ctypes.byref(entry))
    finally:
        kernel32.CloseHandle(snapshot)
    return rows


def _unix_process_rss() -> dict[int, tuple[int, int]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss="],
        check=False,
        capture_output=True,
        text=True,
    )
    rows: dict[int, tuple[int, int]] = {}
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) == 3 and all(part.isdigit() for part in parts):
            pid, ppid, rss_kib = map(int, parts)
            rows[pid] = (ppid, rss_kib * 1024)
    return rows


def process_tree_rss(root_pid: int) -> int:
    rows = _windows_process_rss() if os.name == "nt" else _unix_process_rss()
    descendants = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, (ppid, _) in rows.items():
            if ppid in descendants and pid not in descendants:
                descendants.add(pid)
                changed = True
    return sum(rows.get(pid, (0, 0))[1] for pid in descendants)


def _monitor_rss(process: subprocess.Popen[str], result: list[int]) -> None:
    peak = 0
    while process.poll() is None:
        peak = max(peak, process_tree_rss(process.pid))
        time.sleep(0.05)
    peak = max(peak, process_tree_rss(process.pid))
    result.append(peak)


def run_build(command: list[str], root: Path) -> tuple[int, int | None, str, str]:
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stdout_file, tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stderr_file:
        started = time.perf_counter_ns()
        process = subprocess.Popen(
            command,
            cwd=root,
            stdout=stdout_file,
            stderr=stderr_file,
            text=True,
        )
        rss_result: list[int] = []
        monitor = threading.Thread(target=_monitor_rss, args=(process, rss_result), daemon=True)
        monitor.start()
        return_code = process.wait()
        monitor.join()
        elapsed_ns = time.perf_counter_ns() - started
        stdout_file.seek(0)
        stderr_file.seek(0)
        stdout = stdout_file.read()
        stderr = stderr_file.read()
    if return_code != 0:
        raise RuntimeError(f"build failed ({return_code})\n{stderr}")
    return elapsed_ns, (rss_result[0] if rss_result else None), stdout, stderr


def build_command(package: str, profile: str, jobs: int | None) -> list[str]:
    command = [
        "soldr",
        "cargo",
        "build",
        "--locked",
        "--package",
        package,
        "--message-format=json-render-diagnostics",
    ]
    if profile != "dev":
        command.extend(["--profile", profile])
    if jobs is not None:
        command.extend(["--jobs", str(jobs)])
    return command


def git_output(root: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True).stdout.strip()


def measure(args: argparse.Namespace) -> dict[str, object]:
    command = build_command(args.package, args.profile, args.jobs)
    run_build(command, args.root)
    selected = list(EDIT_SURFACES) if args.surface == "all" else [args.surface]
    all_samples: dict[str, list[BuildSample]] = {}
    for surface in selected:
        relative = EDIT_SURFACES[surface]
        path = args.root / relative
        original = path.stat()
        samples: list[BuildSample] = []
        try:
            for sample_number in range(1, args.samples + 1):
                time.sleep(args.touch_delay)
                os.utime(path, None)
                elapsed_ns, peak_rss, stdout, _ = run_build(command, args.root)
                samples.append(
                    BuildSample(
                        surface=surface,
                        sample=sample_number,
                        elapsed_ns=elapsed_ns,
                        peak_rss_bytes=peak_rss,
                        rebuilt_packages=parse_rebuilt_packages(stdout),
                        touched_path=relative.as_posix(),
                    )
                )
        finally:
            os.utime(path, ns=(original.st_atime_ns, original.st_mtime_ns))
        all_samples[surface] = samples
    return {
        "schema_version": 1,
        "commit": git_output(args.root, "rev-parse", "HEAD"),
        "dirty": bool(git_output(args.root, "status", "--porcelain")),
        "platform": platform.platform(),
        "profile": args.profile,
        "package": args.package,
        "command": command,
        "samples": {name: [asdict(sample) for sample in values] for name, values in all_samples.items()},
        "summary": {name: summarize(values) for name, values in all_samples.items()},
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--surface", choices=["all", *EDIT_SURFACES], default="all")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--touch-delay", type=float, default=1.05)
    parser.add_argument("--package", default="zccache")
    parser.add_argument("--profile", default="dev")
    parser.add_argument("--jobs", type=int)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    return args


def main() -> int:
    args = parse_args()
    result = measure(args)
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
