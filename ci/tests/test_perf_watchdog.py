from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from ci import perf_watchdog


def _run(
    tmp_path: Path, code: str, config: perf_watchdog.WatchdogConfig
) -> tuple[perf_watchdog.CommandResult, str, list[str]]:
    log_path = tmp_path / "attempt.log"
    cache_dir = tmp_path / "cache"
    cache_dir.mkdir()
    statuses: list[str] = []
    with log_path.open("w", encoding="utf-8", buffering=1) as log:
        result = perf_watchdog.run_streamed_command(
            [sys.executable, "-c", code],
            cwd=tmp_path,
            env=os.environ.copy(),
            log=log,
            output_dir=tmp_path / "output",
            cache_dir=cache_dir,
            label="synthetic-child",
            config=config,
            status=statuses.append,
        )
    return result, log_path.read_text(encoding="utf-8"), statuses


def test_streamed_child_output_is_written_to_durable_log(tmp_path: Path) -> None:
    config = perf_watchdog.WatchdogConfig(
        diagnostic_after_seconds=10,
        timeout_seconds=20,
        heartbeat_seconds=10,
        console_output="lite",
        enable_debugger=False,
    )
    result, log, _ = _run(
        tmp_path,
        "import time; print('first', flush=True); time.sleep(0.05); print('second')",
        config,
    )
    assert result.returncode == 0
    assert not result.timed_out
    assert log.splitlines() == ["first", "second"]


def test_timeout_captures_summary_and_terminates_child(tmp_path: Path) -> None:
    config = perf_watchdog.WatchdogConfig(
        diagnostic_after_seconds=0.05,
        timeout_seconds=0.2,
        heartbeat_seconds=0.05,
        console_output="lite",
        debugger_timeout_seconds=0.1,
        enable_debugger=False,
    )
    result, log, statuses = _run(
        tmp_path,
        "import time; print('waiting', flush=True); time.sleep(30)",
        config,
    )
    assert result == perf_watchdog.CommandResult(
        perf_watchdog.TIMEOUT_EXIT_CODE, timed_out=True
    )
    assert "waiting" in log
    summaries = list((tmp_path / "output" / "diagnostics").glob("*/summary.json"))
    assert len(summaries) == 1
    assert any("symbolic stacks" in message for message in statuses)
    assert any("terminating" in message for message in statuses)


def test_environment_configuration_uses_confirmed_60_and_75_minute_windows() -> None:
    config = perf_watchdog.config_from_env(
        {
            "PERF_GUARD_DIAGNOSTIC_AFTER_SECONDS": "3600",
            "PERF_GUARD_TIMEOUT_SECONDS": "4500",
            "PERF_GUARD_HEARTBEAT_SECONDS": "300",
            "PERF_GUARD_CONSOLE_OUTPUT": "lite",
        }
    )
    assert config.diagnostic_after_seconds == 3600
    assert config.timeout_seconds == 4500
    assert config.heartbeat_seconds == 300
    assert config.console_output == "lite"


def test_lite_console_keeps_phase_lines_but_filters_verbose_output() -> None:
    assert perf_watchdog._is_lite_console_line("  [3/3] zccache")
    assert perf_watchdog._is_lite_console_line("        multi warm: starting")
    assert perf_watchdog._is_lite_console_line("## C++ Benchmark")
    assert not perf_watchdog._is_lite_console_line("verbose internal diagnostic")


def test_debugger_targets_root_and_zccache_descendants(tmp_path: Path, monkeypatch) -> None:
    proc_root = tmp_path / "proc"
    (proc_root / "11").mkdir(parents=True)
    (proc_root / "12").mkdir()
    (proc_root / "13").mkdir()
    (proc_root / "12" / "cmdline").write_bytes(b"/usr/bin/clang\0-c\0")
    (proc_root / "13" / "cmdline").write_bytes(b"/tmp/zccache\0daemon\0")

    real_path = perf_watchdog.Path

    def fake_path(value):
        if value == "/proc":
            return proc_root
        return real_path(value)

    monkeypatch.setattr(perf_watchdog, "Path", fake_path)
    assert perf_watchdog._debugger_target_pids(11, [11, 12, 13]) == [11, 13]


def test_diagnostic_failure_cannot_prevent_hard_timeout(tmp_path: Path, monkeypatch) -> None:
    def fail_capture(*args, **kwargs):
        raise OSError("rotating log disappeared")

    monkeypatch.setattr(perf_watchdog, "capture_hang_diagnostics", fail_capture)
    config = perf_watchdog.WatchdogConfig(
        diagnostic_after_seconds=0.05,
        timeout_seconds=0.2,
        heartbeat_seconds=10,
        console_output="lite",
        enable_debugger=False,
    )

    result, _, statuses = _run(
        tmp_path,
        "import time; time.sleep(30)",
        config,
    )

    assert result.timed_out
    assert any("diagnostic capture failed" in message for message in statuses)


def test_hard_timeout_does_not_repeat_full_diagnostic_capture(
    tmp_path: Path, monkeypatch
) -> None:
    captures: list[str] = []

    def record_capture(*args, **kwargs):
        captures.append(kwargs["reason"])
        return tmp_path / "diagnostics"

    monkeypatch.setattr(perf_watchdog, "capture_hang_diagnostics", record_capture)
    config = perf_watchdog.WatchdogConfig(
        diagnostic_after_seconds=0.05,
        timeout_seconds=0.2,
        heartbeat_seconds=10,
        console_output="lite",
        enable_debugger=False,
    )

    result, _, _ = _run(tmp_path, "import time; time.sleep(30)", config)

    assert result.timed_out
    assert captures == ["diagnostic-threshold"]


@pytest.mark.skipif(os.name == "nt", reason="Unix process-group regression")
def test_root_exit_with_inherited_output_is_bounded(tmp_path: Path) -> None:
    config = perf_watchdog.WatchdogConfig(
        diagnostic_after_seconds=10,
        timeout_seconds=20,
        heartbeat_seconds=10,
        output_drain_grace_seconds=0.1,
        console_output="lite",
        enable_debugger=False,
    )
    code = (
        "import subprocess, sys; "
        "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])"
    )

    result, _, statuses = _run(tmp_path, code, config)

    assert result.returncode == 0
    assert any("descendants kept output open" in message for message in statuses)


def test_preserve_logs_reads_versioned_and_benchmark_sidecar_roots(tmp_path: Path) -> None:
    configured = tmp_path / "configured"
    benchmark = tmp_path / "benchmark"
    (configured / "v1" / "logs").mkdir(parents=True)
    (configured / "v1" / "logs" / "configured.log").write_text("configured")
    (benchmark / "logs").mkdir(parents=True)
    (benchmark / "logs" / "benchmark.log").write_text("benchmark")
    sidecar = tmp_path / "runtime-roots.txt"
    sidecar.write_text(f"{benchmark}\n", encoding="utf-8")
    destination = tmp_path / "artifact"

    perf_watchdog.preserve_zccache_logs(
        configured, destination, runtime_roots_file=sidecar
    )

    assert list(destination.rglob("configured.log"))
    assert list(destination.rglob("benchmark.log"))
