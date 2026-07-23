"""Durable output streaming and live-hang diagnostics for perf-guard children."""

from __future__ import annotations

import json
import os
import queue
import re
import shutil
import signal
import subprocess
import threading
import time
from collections import deque
from dataclasses import dataclass
from pathlib import Path
from typing import IO, Callable


TIMEOUT_EXIT_CODE = 124


@dataclass(frozen=True)
class WatchdogConfig:
    diagnostic_after_seconds: float = 60.0 * 60.0
    timeout_seconds: float = 75.0 * 60.0
    heartbeat_seconds: float = 5.0 * 60.0
    console_output: str = "full"
    debugger_timeout_seconds: float = 30.0
    enable_debugger: bool = True
    output_drain_grace_seconds: float = 2.0


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    timed_out: bool


def config_from_env(env: dict[str, str] | None = None) -> WatchdogConfig:
    values = os.environ if env is None else env

    def number(name: str, default: float) -> float:
        value = float(values.get(name, default))
        if value < 0:
            raise ValueError(f"{name} must be non-negative")
        return value

    console_output = values.get("PERF_GUARD_CONSOLE_OUTPUT", "full")
    if console_output not in ("full", "lite"):
        raise ValueError("PERF_GUARD_CONSOLE_OUTPUT must be 'full' or 'lite'")
    return WatchdogConfig(
        diagnostic_after_seconds=number("PERF_GUARD_DIAGNOSTIC_AFTER_SECONDS", 3600),
        timeout_seconds=number("PERF_GUARD_TIMEOUT_SECONDS", 4500),
        heartbeat_seconds=number("PERF_GUARD_HEARTBEAT_SECONDS", 300),
        console_output=console_output,
    )


def run_streamed_command(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    log: IO[str],
    output_dir: Path,
    cache_dir: Path,
    label: str,
    config: WatchdogConfig,
    status: Callable[[str], None],
) -> CommandResult:
    """Run one child while continuously flushing its merged output to disk."""
    popen_options: dict[str, object] = {}
    if os.name == "nt":
        popen_options["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        popen_options["start_new_session"] = True

    runtime_roots_file = _runtime_roots_file(output_dir, label)
    runtime_roots_file.parent.mkdir(parents=True, exist_ok=True)
    runtime_roots_file.unlink(missing_ok=True)
    child_env = env.copy()
    child_env["PERF_GUARD_RUNTIME_ROOTS_FILE"] = str(runtime_roots_file)

    with subprocess.Popen(
        command,
        cwd=cwd,
        env=child_env,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=1,
        **popen_options,
    ) as proc:
        assert proc.stdout is not None
        lines: queue.Queue[str | None] = queue.Queue()

        def read_output() -> None:
            try:
                for line in proc.stdout:
                    lines.put(line)
            finally:
                lines.put(None)

        reader = threading.Thread(target=read_output, name="perf-output-reader", daemon=True)
        reader.start()
        started = time.monotonic()
        last_output = started
        next_heartbeat = started + config.heartbeat_seconds
        diagnostic_at = started + config.diagnostic_after_seconds
        timeout_at = started + config.timeout_seconds
        diagnostic_taken = False
        reader_done = False
        root_exited_at: float | None = None
        last_line = "<no child output>"
        recent_lines: deque[str] = deque(maxlen=40)

        try:
            while True:
                now = time.monotonic()
                try:
                    item = lines.get(timeout=0.25)
                except queue.Empty:
                    item = ""
                if item is None:
                    reader_done = True
                elif item:
                    log.write(item)
                    log.flush()
                    last_output = now
                    last_line = item.rstrip()[-500:]
                    recent_lines.append(item.rstrip())
                    if config.console_output == "full" or _is_lite_console_line(item):
                        print(item, end="", flush=True)

                running = proc.poll() is None
                if not diagnostic_taken and now >= diagnostic_at and running:
                    diagnostic_taken = True
                    phase = _infer_phase(recent_lines)
                    _safe_status(
                        status,
                        f"{label} is still running after {_elapsed(now - started)}; "
                        f"last phase={phase!r}; capturing logs and symbolic stacks",
                    )
                    try:
                        capture_hang_diagnostics(
                            proc.pid,
                            command=command,
                            output_dir=output_dir,
                            cache_dir=cache_dir,
                            runtime_roots_file=runtime_roots_file,
                            label=label,
                            reason="diagnostic-threshold",
                            elapsed_seconds=now - started,
                            silent_seconds=now - last_output,
                            last_line=last_line,
                            recent_output=list(recent_lines),
                            config=config,
                            status=status,
                        )
                    except Exception as error:  # diagnostics must never defeat timeout cleanup
                        _safe_status(status, f"hang diagnostic capture failed: {error}")
                    try:
                        _append_step_summary(
                            f"### Perf watchdog: {label}\n\n"
                            f"Still running after {_elapsed(now - started)}. Live stacks and full "
                            f"logs are in the perf-guard artifact. Last phase: `{phase}`. "
                            f"Last output: `{last_line}`\n"
                        )
                    except OSError as error:
                        _safe_status(status, f"step-summary write failed: {error}")

                if now >= next_heartbeat and running:
                    _safe_status(
                        status,
                        f"{label} heartbeat: elapsed={_elapsed(now - started)}, "
                        f"silent={_elapsed(now - last_output)}, last={last_line!r}",
                    )
                    next_heartbeat = now + config.heartbeat_seconds

                if now >= timeout_at and running:
                    _safe_status(
                        status,
                        f"{label} exceeded {_elapsed(config.timeout_seconds)}; "
                        "terminating its process group immediately",
                    )
                    _terminate_process_group(proc, status)
                    reader.join(timeout=5)
                    _drain_output_queue(lines, log, config.console_output)
                    return CommandResult(TIMEOUT_EXIT_CODE, timed_out=True)

                returncode = proc.poll()
                if returncode is not None:
                    if root_exited_at is None:
                        root_exited_at = now
                    if reader_done and lines.empty():
                        reader.join(timeout=1)
                        return CommandResult(returncode, timed_out=False)
                    if now - root_exited_at >= config.output_drain_grace_seconds:
                        _safe_status(
                            status,
                            f"{label} root exited but descendants kept output open; "
                            "terminating the remaining process group",
                        )
                        _terminate_process_group(proc, status)
                        reader.join(timeout=5)
                        _drain_output_queue(lines, log, config.console_output)
                        return CommandResult(returncode, timed_out=False)
        except BaseException:
            _terminate_process_group(proc, status)
            reader.join(timeout=5)
            _drain_output_queue(lines, log, config.console_output)
            raise


def capture_hang_diagnostics(
    root_pid: int,
    *,
    command: list[str],
    output_dir: Path,
    cache_dir: Path,
    runtime_roots_file: Path,
    label: str,
    reason: str,
    elapsed_seconds: float,
    silent_seconds: float,
    last_line: str,
    recent_output: list[str],
    config: WatchdogConfig,
    status: Callable[[str], None],
) -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    destination = output_dir / "diagnostics" / f"{_safe_name(label)}-{reason}-{stamp}"
    destination.mkdir(parents=True, exist_ok=True)
    pids = _process_tree(root_pid)
    summary = {
        "reason": reason,
        "root_pid": root_pid,
        "pids": pids,
        "elapsed_seconds": elapsed_seconds,
        "silent_seconds": silent_seconds,
        "last_line": last_line,
        "inferred_phase": _infer_phase(recent_output),
        "recent_output": recent_output,
        "command": command,
    }
    (destination / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )
    (destination / "summary.md").write_text(
        "# Perf hang snapshot\n\n"
        f"- Reason: `{reason}`\n"
        f"- Root PID: `{root_pid}`\n"
        f"- Elapsed: `{_elapsed(elapsed_seconds)}`\n"
        f"- Time since output: `{_elapsed(silent_seconds)}`\n"
        f"- Processes: `{', '.join(map(str, pids)) or 'none'}`\n"
        f"- Inferred phase: `{_infer_phase(recent_output)}`\n"
        f"- Last output: `{last_line}`\n\n"
        "## Recent output\n\n```text\n"
        + "\n".join(recent_output)
        + "\n```\n",
        encoding="utf-8",
    )
    _capture_process_snapshot(pids, destination)
    preserve_zccache_logs(
        cache_dir,
        destination / "zccache-runtime",
        runtime_roots_file=runtime_roots_file,
    )

    debugger = shutil.which("gdb") if config.enable_debugger else None
    if debugger is None:
        status("live debugger unavailable; /proc and process-tree snapshots were retained")
    else:
        for pid in _debugger_target_pids(root_pid, pids):
            status(f"capturing all-thread symbolic stack for PID {pid}")
            stack_path = destination / f"gdb-{pid}.txt"
            debugger_exit: int | None = None
            with stack_path.open("w", encoding="utf-8", errors="replace") as output:
                try:
                    result = subprocess.run(
                        [
                            debugger,
                            "-batch",
                            "-nx",
                            "-ex",
                            "set pagination off",
                            "-ex",
                            "set print thread-events off",
                            "-ex",
                            "thread apply all bt full",
                            "-p",
                            str(pid),
                        ],
                        stdout=output,
                        stderr=subprocess.STDOUT,
                        check=False,
                        timeout=config.debugger_timeout_seconds,
                    )
                    debugger_exit = result.returncode
                    output.write(f"\n[gdb exit={result.returncode}]\n")
                except subprocess.TimeoutExpired:
                    output.write("\n[gdb timed out and was terminated]\n")
            stack_text = stack_path.read_text(encoding="utf-8", errors="replace")
            if debugger_exit != 0 or not re.search(r"(^|\n)(Thread\s|#0\s)", stack_text):
                _safe_status(
                    status,
                    f"symbolic stack capture for PID {pid} was incomplete "
                    f"(gdb exit={debugger_exit})",
                )
    return destination


def preserve_zccache_logs(
    cache_dir: Path,
    destination: Path,
    *,
    runtime_roots_file: Path | None = None,
) -> None:
    """Copy diagnostic metadata without uploading the potentially huge cache."""
    copied = False
    roots = [cache_dir]
    if runtime_roots_file is not None:
        try:
            roots.extend(
                Path(line.strip())
                for line in runtime_roots_file.read_text(encoding="utf-8").splitlines()
                if line.strip()
            )
        except OSError:
            pass
    unique_roots = list(dict.fromkeys(root.resolve() for root in roots))
    for root_index, root in enumerate(unique_roots):
        candidates = [root]
        try:
            candidates.extend(
                child
                for child in root.iterdir()
                if child.is_dir() and child.name.startswith("v")
            )
        except OSError:
            pass
        for candidate in candidates:
            target = destination / f"root-{root_index}"
            if candidate != root:
                target /= candidate.name
            for name in ("logs", "crashes", "daemon"):
                source = candidate / name
                if source.exists():
                    try:
                        shutil.copytree(source, target / name, dirs_exist_ok=True)
                        copied = True
                    except OSError:
                        pass
            for pattern in ("*.json", "*.jsonl", "*.log", "*.symref"):
                for source in candidate.glob(pattern):
                    try:
                        target.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(source, target / source.name)
                        copied = True
                    except OSError:
                        pass
    if not copied:
        destination.mkdir(parents=True, exist_ok=True)
        (destination / "README.txt").write_text(
            "No zccache runtime logs or crash records existed at capture time.\n",
            encoding="utf-8",
        )


def preserve_runtime_logs_and_remove_cache(
    cache_dir: Path, output_dir: Path, label: str
) -> None:
    preserve_zccache_logs(
        cache_dir,
        output_dir / "runtime-logs" / label,
        runtime_roots_file=_runtime_roots_file(output_dir, label),
    )
    shutil.rmtree(cache_dir, ignore_errors=True)


def _capture_process_snapshot(pids: list[int], destination: Path) -> None:
    if pids and shutil.which("ps"):
        with (destination / "process-tree.txt").open("w", encoding="utf-8") as output:
            subprocess.run(
                [
                    "ps",
                    "-ww",
                    "-o",
                    "pid,ppid,stat,etime,wchan:32,comm,args",
                    "-p",
                    ",".join(map(str, pids)),
                ],
                stdout=output,
                stderr=subprocess.STDOUT,
                check=False,
            )
    for pid in pids:
        proc_dir = Path("/proc") / str(pid)
        pid_dir = destination / f"proc-{pid}"
        pid_dir.mkdir(parents=True, exist_ok=True)
        for name in ("cmdline", "status", "wchan", "stack", "maps"):
            try:
                data = (proc_dir / name).read_bytes()
            except OSError as error:
                data = f"unavailable: {error}\n".encode()
            if name == "cmdline":
                data = data.replace(b"\0", b" ")
            (pid_dir / f"{name}.txt").write_bytes(data)


def _process_tree(root_pid: int) -> list[int]:
    if os.name == "nt" or not Path("/proc").is_dir():
        return [root_pid]
    children: dict[int, list[int]] = {}
    for proc_dir in Path("/proc").iterdir():
        if not proc_dir.name.isdigit():
            continue
        try:
            stat = (proc_dir / "stat").read_text(encoding="utf-8")
            fields = stat[stat.rfind(")") + 2 :].split()
            parent = int(fields[1])
            pid = int(proc_dir.name)
        except (OSError, ValueError, IndexError):
            continue
        children.setdefault(parent, []).append(pid)
    result: list[int] = []
    pending = [root_pid]
    while pending:
        pid = pending.pop(0)
        if pid in result:
            continue
        if pid == root_pid or (Path("/proc") / str(pid)).exists():
            result.append(pid)
            pending.extend(children.get(pid, ()))
    return result


def _debugger_target_pids(root_pid: int, pids: list[int]) -> list[int]:
    """Limit live attaches to the benchmark root and zccache processes.

    The process snapshot still covers the full tree. Avoiding every compiler
    child keeps a large tree from pushing the 75-minute termination deadline
    back by one debugger timeout per process.
    """
    targets = [root_pid]
    for pid in pids:
        if pid == root_pid:
            continue
        try:
            command = (Path("/proc") / str(pid) / "cmdline").read_bytes().lower()
        except OSError:
            continue
        if b"zccache" in command:
            targets.append(pid)
    return targets[:4]


def _terminate_process_group(
    proc: subprocess.Popen[str], status: Callable[[str], None]
) -> None:
    try:
        if os.name == "nt":
            subprocess.run(
                ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=10,
            )
        else:
            os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=10)
        return
    except (OSError, subprocess.TimeoutExpired):
        _safe_status(status, "graceful termination did not finish; forcing process-group shutdown")
    try:
        if os.name == "nt":
            proc.kill()
        else:
            os.killpg(proc.pid, signal.SIGKILL)
    except OSError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        _safe_status(status, f"PID {proc.pid} survived forced shutdown")


def _safe_status(status: Callable[[str], None], message: str) -> None:
    try:
        status(message)
    except Exception:
        pass


def _runtime_roots_file(output_dir: Path, label: str) -> Path:
    return output_dir / "runtime-roots" / f"{_safe_name(label)}.txt"


def _drain_output_queue(
    lines: queue.Queue[str | None], log: IO[str], console_output: str
) -> None:
    while True:
        try:
            item = lines.get_nowait()
        except queue.Empty:
            break
        if not item:
            continue
        log.write(item)
        log.flush()
        if console_output == "full" or _is_lite_console_line(item):
            print(item, end="", flush=True)


def _is_lite_console_line(line: str) -> bool:
    text = line.strip()
    lower = text.lower()
    return (
        text.startswith(("===", "---", "test result:", "running "))
        or (text.startswith("test ") and text.endswith(("... ok", "... FAILED")))
        or "benchmark" in lower
        or re.match(r"^\[\d+/\d+\]", text) is not None
        or any(marker in lower for marker in ("single cold:", "single warm:", "multi cold:", "multi warm:"))
        or text.startswith(("SKIP:", "FAILED", "error:"))
    )


def _append_step_summary(markdown: str) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with Path(path).open("a", encoding="utf-8") as summary:
            summary.write(markdown.rstrip() + "\n\n")


def _elapsed(seconds: float) -> str:
    total = max(0, int(seconds))
    minutes, secs = divmod(total, 60)
    return f"{minutes}m{secs:02d}s"


def _safe_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-") or "command"


def _infer_phase(lines: list[str] | deque[str]) -> str:
    for line in reversed(lines):
        text = line.strip()
        if not text:
            continue
        if text.startswith(("===", "---")) or any(
            marker in text.lower()
            for marker in ("cold", "warm", "multi-file", "single-file", "benchmark")
        ):
            return text[-200:]
    return lines[-1][-200:] if lines else "no output"
