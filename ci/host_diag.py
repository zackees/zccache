"""Single-mode host validation diagnostic (issue #1186).

Usage:
    uv run python -m ci.host_diag                 # run the fixed gate sequence
    uv run python -m ci.host_diag run --skip-lint  # skip the lint gate
    uv run python -m ci.host_diag scenario no-change-pair  # two back-to-back
                                                             # no-op builds

Runs a fixed sequence of diagnostic gates (lint, unit build, unit test,
integration test), streams each gate's merged stdout/stderr to both the
console and a per-gate log file with elapsed-time prefixes, snapshots and
summarizes the zccache compile journal across each gate's wall-clock window,
best-effort scans soldr's auto-gc log, and writes a single JSON report.

This is a diagnostic tool, not a CI gate: a failing gate is recorded and the
sequence continues so later gates (and their journal snapshots) still run.
The process exit code is nonzero if any gate failed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from ci.env import clean_env
from ci.soldr import self_build_env, soldr_executable

# --------------------------------------------------------------------------
# Constants
# --------------------------------------------------------------------------

REPO_ROOT = Path(__file__).parent.parent.resolve()
DEFAULT_OUTPUT_DIR = REPO_ROOT / ".cache" / "host-diag"

ENV_CAPTURE_TIMEOUT_S = 30

# Env vars whose presence/value is diagnostically relevant (null if unset).
CAPTURED_ENV_VARS = (
    "SOLDR_RUSTC_WRAPPER",
    "ZCCACHE_DAEMON_NAMESPACE",
    "ZCCACHE_CACHE_DIR",
    "CARGO_TARGET_DIR",
)

# The compile journal's `ts` field is an ISO 8601 UTC string with millisecond
# precision (see docs/journal-schema.md + zccache_daemon_core::daemon::
# event_log::format_timestamp, e.g. "2026-07-23T12:34:56.789Z"), NOT a
# numeric field. summarize_journal()'s fixed API operates on numeric
# `ts` (matching window_start_ns/window_end_ns units); we convert every
# record's string ts to epoch nanoseconds before handing records to
# summarize_journal so the window comparison is well-defined.
JOURNAL_TS_FORMAT = "%Y-%m-%dT%H:%M:%S.%fZ"

# Candidate auto-gc log locations (soldr side), best-effort.
AUTO_GC_LOG_CANDIDATES = (
    Path.home() / ".soldr-dev" / "logs" / "auto-gc.log",
    Path.home() / ".soldr" / "logs" / "auto-gc.log",
)

# outcomes that count as hits / misses for hit-rate purposes (mirrors
# summarize_journal's own bucketing so the terminal summary table agrees).
HIT_OUTCOMES = ("hit", "link_hit")
MISS_OUTCOMES = ("miss", "link_miss")


# --------------------------------------------------------------------------
# Fixed public API (issue #1186) — exact signatures, do not rename.
# --------------------------------------------------------------------------


def new_run_id(now_utc: datetime, entropy: str) -> str:
    return f"hd-{now_utc:%Y%m%dT%H%M%S}Z-{entropy[:8]}"


def format_stream_line(elapsed_ns: int, text: str) -> str:
    return f"[+{elapsed_ns / 1_000_000_000:012.6f}s] {text}"


def parse_journal_lines(lines: Any) -> "tuple[list[dict], int]":
    records: list[dict] = []
    malformed = 0
    for line in lines:
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            malformed += 1
            continue
        if isinstance(value, dict):
            records.append(value)
        else:
            malformed += 1
    return records, malformed


def summarize_journal(
    records: "list[dict]", window_start_ns: int, window_end_ns: int
) -> dict:
    in_window = [
        record
        for record in records
        if isinstance(record.get("ts"), (int, float))
        and window_start_ns <= record["ts"] <= window_end_ns
    ]

    grouped: "dict[Any, list[dict]]" = {}
    for record in in_window:
        grouped.setdefault(record.get("session_id"), []).append(record)

    sessions = [
        _summarize_group(session_id, group) for session_id, group in grouped.items()
    ]
    sessions.sort(key=lambda entry: entry["first_ts"])

    overlapping = _sessions_overlap(sessions)

    aggregate = None
    aggregate_rejected_reason = None
    if overlapping:
        aggregate_rejected_reason = "overlapping-sessions"
    elif in_window:
        aggregate = _summarize_group(None, in_window)
        aggregate.pop("session_id", None)

    return {
        "total": len(records),
        "in_window": len(in_window),
        "sessions": sessions,
        "overlapping": overlapping,
        "aggregate": aggregate,
        "aggregate_rejected_reason": aggregate_rejected_reason,
    }


def scan_auto_gc_log(lines: Any) -> dict:
    starts = 0
    terminals = 0
    for line in lines:
        if "stage=start" in line:
            starts += 1
        if "stage=done" in line or "detected" in line or "warning" in line:
            terminals += 1
    return {
        "starts": starts,
        "terminals": terminals,
        "unterminated": max(0, starts - terminals),
    }


def build_report(
    run_id: str,
    environment: dict,
    gates: "list[dict]",
    journal: "dict | None",
    auto_gc: "dict | None",
) -> dict:
    return {
        "schema_version": 1,
        "run_id": run_id,
        "environment": environment,
        "gates": gates,
        "journal": journal,
        "auto_gc": auto_gc,
    }


# --------------------------------------------------------------------------
# Internal helpers for summarize_journal
# --------------------------------------------------------------------------


def _summarize_group(session_id: Any, group: "list[dict]") -> dict:
    timestamps = [record["ts"] for record in group]
    outcomes: "dict[str, int]" = {}
    miss_reasons: "dict[str, int]" = {}
    for record in group:
        outcome = record.get("outcome")
        if outcome is not None:
            outcomes[outcome] = outcomes.get(outcome, 0) + 1
        miss_reason = record.get("miss_reason")
        if miss_reason is not None:
            miss_reasons[miss_reason] = miss_reasons.get(miss_reason, 0) + 1

    hits = sum(outcomes.get(name, 0) for name in HIT_OUTCOMES)
    misses = sum(outcomes.get(name, 0) for name in MISS_OUTCOMES)
    hit_rate = hits / (hits + misses) if (hits + misses) > 0 else None

    return {
        "session_id": session_id,
        "first_ts": min(timestamps),
        "last_ts": max(timestamps),
        "records": len(group),
        "outcomes": outcomes,
        "miss_reasons": miss_reasons,
        "hit_rate": hit_rate,
    }


def _sessions_overlap(sessions: "list[dict]") -> bool:
    # sessions is sorted by first_ts; adjacent-pair check suffices for
    # interval overlap once sorted, but compare all pairs to be safe against
    # ties / out-of-order last_ts.
    for i in range(len(sessions)):
        for j in range(i + 1, len(sessions)):
            a, b = sessions[i], sessions[j]
            if a["first_ts"] <= b["last_ts"] and b["first_ts"] <= a["last_ts"]:
                return True
    return False


# --------------------------------------------------------------------------
# Journal timestamp conversion (real journal records only; not part of the
# fixed summarize_journal API, which already expects numeric ts).
# --------------------------------------------------------------------------


def _journal_ts_to_epoch_ns(value: str) -> "int | None":
    try:
        parsed = datetime.strptime(value, JOURNAL_TS_FORMAT).replace(tzinfo=timezone.utc)
    except ValueError:
        return None
    return int(parsed.timestamp() * 1_000_000_000)


def _numericize_journal_records(records: "list[dict]") -> "list[dict]":
    """Return copies of records with a numeric `ts` (epoch ns) field.

    Records whose `ts` cannot be parsed are dropped from the numeric window
    entirely (summarize_journal already excludes non-numeric ts).
    """
    numeric: "list[dict]" = []
    for record in records:
        raw_ts = record.get("ts")
        if not isinstance(raw_ts, str):
            continue
        epoch_ns = _journal_ts_to_epoch_ns(raw_ts)
        if epoch_ns is None:
            continue
        converted = dict(record)
        converted["ts"] = epoch_ns
        numeric.append(converted)
    return numeric


# --------------------------------------------------------------------------
# Environment capture
# --------------------------------------------------------------------------


def _run_capture(cmd: "list[str]", *, env: "dict[str, str] | None" = None) -> dict:
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            cwd=str(REPO_ROOT),
            env=env if env is not None else clean_env(),
            timeout=ENV_CAPTURE_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"error": str(error)}
    return {
        "exit_code": result.returncode,
        "stdout": result.stdout.strip(),
        "stderr": result.stderr.strip(),
    }


def _capture_json_cmd(cmd: "list[str]") -> dict:
    captured = _run_capture(cmd)
    if "error" in captured:
        return captured
    if captured["exit_code"] != 0:
        return {"error": f"exit {captured['exit_code']}: {captured['stderr']}"}
    try:
        return json.loads(captured["stdout"])
    except (json.JSONDecodeError, ValueError) as error:
        return {"error": f"invalid JSON: {error}"}


def capture_environment() -> dict:
    environment: "dict[str, Any]" = {
        "captured_utc": datetime.now(timezone.utc).isoformat(),
        "platform": sys.platform,
    }

    try:
        soldr = soldr_executable()
        environment["soldr_path"] = soldr
        environment["soldr_version"] = _run_capture([soldr, "--version"])
    except SystemExit as error:
        environment["soldr_path"] = None
        environment["soldr_version"] = {"error": str(error)}

    zccache = shutil.which("zccache")
    if zccache is None:
        environment["zccache_cache_root"] = {"error": "zccache not found on PATH"}
        environment["zccache_status"] = {"error": "zccache not found on PATH"}
    else:
        environment["zccache_cache_root"] = _capture_json_cmd(
            [zccache, "cache-root", "--json"]
        )
        environment["zccache_status"] = _capture_json_cmd([zccache, "status", "--json"])

    try:
        soldr = soldr_executable()
        environment["rustc_vv"] = _run_capture(
            [soldr, "--no-cache", "rustc", "-vV"], env=self_build_env()
        )
    except SystemExit as error:
        environment["rustc_vv"] = {"error": str(error)}

    environment["git_head"] = _run_capture(["git", "rev-parse", "HEAD"])

    porcelain = _run_capture(["git", "status", "--porcelain"])
    if "error" in porcelain:
        environment["git_tree_fingerprint"] = porcelain
    else:
        digest = hashlib.sha256(porcelain["stdout"].encode("utf-8")).hexdigest()
        environment["git_tree_fingerprint"] = digest

    try:
        usage = shutil.disk_usage(str(REPO_ROOT))
        environment["disk_free_bytes"] = usage.free
    except OSError as error:
        environment["disk_free_bytes"] = {"error": str(error)}

    environment["env_vars"] = {
        name: os.environ.get(name) for name in CAPTURED_ENV_VARS
    }

    return environment


def _cache_root_from_environment(environment: dict) -> "Path | None":
    cache_root_info = environment.get("zccache_cache_root")
    if not isinstance(cache_root_info, dict):
        return None
    value = cache_root_info.get("cache_root")
    if not isinstance(value, str) or not value:
        return None
    return Path(value)


# --------------------------------------------------------------------------
# Auto-GC scan
# --------------------------------------------------------------------------


def scan_auto_gc() -> dict:
    found: "list[str]" = []
    starts = 0
    terminals = 0
    for candidate in AUTO_GC_LOG_CANDIDATES:
        if not candidate.is_file():
            continue
        found.append(str(candidate))
        try:
            lines = candidate.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        partial = scan_auto_gc_log(lines)
        starts += partial["starts"]
        terminals += partial["terminals"]
    return {
        "paths_found": found,
        "starts": starts,
        "terminals": terminals,
        "unterminated": max(0, starts - terminals),
    }


# --------------------------------------------------------------------------
# Gate runner
# --------------------------------------------------------------------------


def _is_soldr_cmd(cmd: "list[str]") -> bool:
    return bool(cmd) and Path(cmd[0]).name.startswith("soldr")


def run_gate(
    name: str,
    cmd: "list[str]",
    *,
    output_dir: Path,
    status: "Any" = print,
) -> dict:
    """Run one diagnostic gate, streaming merged output with elapsed prefixes."""
    log_path = output_dir / f"{name}.stream.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)

    env = self_build_env() if _is_soldr_cmd(cmd) else clean_env()
    started_monotonic_ns = time.monotonic_ns()
    started_utc = datetime.now(timezone.utc)

    status(f"[host-diag] gate '{name}' starting: {' '.join(cmd)}")

    line_count = 0
    exit_code: int
    with log_path.open("w", encoding="utf-8", errors="replace") as log_file:
        try:
            with subprocess.Popen(
                cmd,
                cwd=str(REPO_ROOT),
                env=env,
                text=True,
                encoding="utf-8",
                errors="replace",
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                bufsize=1,
            ) as proc:
                assert proc.stdout is not None
                for raw_line in proc.stdout:
                    elapsed_ns = time.monotonic_ns() - started_monotonic_ns
                    formatted = format_stream_line(elapsed_ns, raw_line.rstrip("\n"))
                    print(formatted)
                    log_file.write(formatted + "\n")
                    log_file.flush()
                    line_count += 1
                proc.wait()
                exit_code = proc.returncode
        except OSError as error:
            elapsed_ns = time.monotonic_ns() - started_monotonic_ns
            formatted = format_stream_line(elapsed_ns, f"error launching gate: {error}")
            print(formatted)
            log_file.write(formatted + "\n")
            line_count += 1
            exit_code = 1

    wall_ns = time.monotonic_ns() - started_monotonic_ns
    status(
        f"[host-diag] gate '{name}' finished: exit={exit_code} "
        f"wall={wall_ns / 1_000_000_000:.3f}s"
    )

    return {
        "name": name,
        "cmd": cmd,
        "exit_code": exit_code,
        "wall_ns": wall_ns,
        "started_utc": started_utc.isoformat().replace("+00:00", "Z"),
        "stream_log": str(log_path),
        "lines": line_count,
    }


def _snapshot_journal_for_gate(
    gate_result: dict, environment: dict
) -> "dict | None":
    cache_root = _cache_root_from_environment(environment)
    if cache_root is None:
        return None
    journal_path = cache_root / "logs" / "compile_journal.jsonl"
    if not journal_path.is_file():
        return None

    try:
        raw_lines = journal_path.read_text(
            encoding="utf-8", errors="replace"
        ).splitlines()
    except OSError as error:
        return {"error": str(error)}

    records, malformed = parse_journal_lines(raw_lines)
    numeric_records = _numericize_journal_records(records)

    started_utc = datetime.fromisoformat(gate_result["started_utc"].replace("Z", "+00:00"))
    window_start_ns = int(started_utc.timestamp() * 1_000_000_000)
    window_end_ns = window_start_ns + gate_result["wall_ns"]

    summary = summarize_journal(numeric_records, window_start_ns, window_end_ns)
    summary["malformed_lines"] = malformed
    summary["journal_path"] = str(journal_path)
    return summary


# --------------------------------------------------------------------------
# Gate command builders
# --------------------------------------------------------------------------


def _lint_cmd() -> "list[str]":
    uv = shutil.which("uv") or "uv"
    return [uv, "run", "python", "-m", "ci.lint"]


def _unit_build_cmd(*, no_cache: bool) -> "list[str]":
    soldr = soldr_executable()
    cmd = [soldr]
    if no_cache:
        cmd.append("--no-cache")
    cmd += ["cargo", "test", "--workspace", "--no-run"]
    return cmd


def _unit_test_cmd(*, no_cache: bool) -> "list[str]":
    soldr = soldr_executable()
    cmd = [soldr]
    if no_cache:
        cmd.append("--no-cache")
    cmd += ["cargo", "test", "--workspace", "--", "--test-threads=1"]
    return cmd


def _integration_test_cmd(*, no_cache: bool) -> "list[str]":
    soldr = soldr_executable()
    cmd = [soldr]
    if no_cache:
        cmd.append("--no-cache")
    cmd += [
        "cargo",
        "test",
        "--workspace",
        "--",
        "--ignored",
        "--skip",
        "bench_",
        "--skip",
        "perf_",
        "--skip",
        "stress_",
        "--skip",
        "_stress",
        "--test-threads=1",
    ]
    return cmd


# --------------------------------------------------------------------------
# Runners
# --------------------------------------------------------------------------


def _run_gate_sequence(
    gates: "list[tuple[str, list[str]]]",
    run_dir: Path,
    environment: dict,
) -> "list[dict]":
    results = []
    for name, cmd in gates:
        gate_result = run_gate(name, cmd, output_dir=run_dir)
        gate_result["journal"] = _snapshot_journal_for_gate(gate_result, environment)
        results.append(gate_result)
    return results


def _write_report(
    run_id: str,
    run_dir: Path,
    environment: dict,
    gate_results: "list[dict]",
    auto_gc: "dict | None",
) -> Path:
    # The top-level `journal` field summarizes the union of gate windows so a
    # single report field answers "what happened across this whole run";
    # per-gate journal summaries remain nested on each gate result.
    combined_journal = None
    gate_journals = [g["journal"] for g in gate_results if g.get("journal")]
    if gate_journals:
        combined_journal = {
            "note": "see gates[*].journal for per-gate windows",
            "gate_windows_summarized": len(gate_journals),
        }

    report = build_report(
        run_id,
        environment,
        [{k: v for k, v in g.items()} for g in gate_results],
        combined_journal,
        auto_gc,
    )
    report_path = run_dir / "report.json"
    report_path.write_text(
        json.dumps(report, indent=2, sort_keys=True, default=str), encoding="utf-8"
    )
    return report_path


def _print_summary_table(gate_results: "list[dict]") -> None:
    print("\n[host-diag] summary")
    print(f"{'gate':<20} {'wall(s)':>10} {'exit':>6} {'hit_rate':>10}")
    for gate in gate_results:
        wall_s = gate["wall_ns"] / 1_000_000_000
        journal = gate.get("journal") or {}
        aggregate = journal.get("aggregate") if isinstance(journal, dict) else None
        hit_rate = aggregate.get("hit_rate") if isinstance(aggregate, dict) else None
        hit_rate_display = f"{hit_rate:.3f}" if isinstance(hit_rate, float) else "n/a"
        print(f"{gate['name']:<20} {wall_s:>10.3f} {gate['exit_code']:>6} {hit_rate_display:>10}")


def cmd_run(args: argparse.Namespace) -> int:
    now_utc = datetime.now(timezone.utc)
    run_id = new_run_id(now_utc, entropy=os.urandom(4).hex())

    output_dir = Path(args.output_dir).resolve()
    run_dir = output_dir / run_id
    run_dir.mkdir(parents=True, exist_ok=True)

    print(f"[host-diag] run_id={run_id} output={run_dir}")

    environment = capture_environment()

    gates: "list[tuple[str, list[str]]]" = []
    if not args.skip_lint:
        gates.append(("lint", _lint_cmd()))
    gates.append(("unit_build", _unit_build_cmd(no_cache=args.no_cache)))
    gates.append(("unit_test", _unit_test_cmd(no_cache=args.no_cache)))
    if not args.skip_integration:
        gates.append(("integration_test", _integration_test_cmd(no_cache=args.no_cache)))

    gate_results = _run_gate_sequence(gates, run_dir, environment)
    auto_gc = scan_auto_gc()

    report_path = _write_report(run_id, run_dir, environment, gate_results, auto_gc)
    print(f"[host-diag] report written to {report_path}")
    _print_summary_table(gate_results)

    return 0 if all(g["exit_code"] == 0 for g in gate_results) else 1


def cmd_scenario_no_change_pair(args: argparse.Namespace) -> int:
    now_utc = datetime.now(timezone.utc)
    run_id = new_run_id(now_utc, entropy=os.urandom(4).hex())

    output_dir = Path(args.output_dir).resolve()
    run_dir = output_dir / run_id
    run_dir.mkdir(parents=True, exist_ok=True)

    print(f"[host-diag] scenario=no-change-pair run_id={run_id} output={run_dir}")

    environment = capture_environment()

    gates = [
        ("warm_build_1", _unit_build_cmd(no_cache=args.no_cache)),
        ("warm_build_2", _unit_build_cmd(no_cache=args.no_cache)),
    ]
    gate_results = _run_gate_sequence(gates, run_dir, environment)
    auto_gc = scan_auto_gc()

    report_path = _write_report(run_id, run_dir, environment, gate_results, auto_gc)
    print(f"[host-diag] report written to {report_path}")
    _print_summary_table(gate_results)

    return 0 if all(g["exit_code"] == 0 for g in gate_results) else 1


# --------------------------------------------------------------------------
# Entrypoint
# --------------------------------------------------------------------------


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="ci.host_diag", description=__doc__
    )
    subparsers = parser.add_subparsers(dest="subcommand")

    run_parser = subparsers.add_parser(
        "run", help="run the fixed diagnostic gate sequence (default)"
    )
    run_parser.add_argument("--skip-integration", action="store_true")
    run_parser.add_argument("--skip-lint", action="store_true")
    run_parser.add_argument(
        "--output-dir", default=str(DEFAULT_OUTPUT_DIR), help="report output directory"
    )
    run_parser.add_argument(
        "--no-cache",
        action="store_true",
        help="pass --no-cache through to soldr invocations",
    )
    run_parser.set_defaults(func=cmd_run)

    scenario_parser = subparsers.add_parser(
        "scenario", help="run a named diagnostic scenario"
    )
    scenario_subparsers = scenario_parser.add_subparsers(dest="scenario_name")
    no_change_pair_parser = scenario_subparsers.add_parser(
        "no-change-pair", help="two back-to-back no-change unit_build gates"
    )
    no_change_pair_parser.add_argument(
        "--output-dir", default=str(DEFAULT_OUTPUT_DIR), help="report output directory"
    )
    no_change_pair_parser.add_argument("--no-cache", action="store_true")
    no_change_pair_parser.set_defaults(func=cmd_scenario_no_change_pair)

    return parser


def main(argv: "list[str] | None" = None) -> int:
    raw_args = sys.argv[1:] if argv is None else argv

    # Default subcommand is `run` when none given (and the first token isn't
    # already a known subcommand or help flag).
    known_subcommands = {"run", "scenario", "-h", "--help"}
    if not raw_args or raw_args[0] not in known_subcommands:
        raw_args = ["run", *raw_args]

    parser = _build_parser()
    args = parser.parse_args(raw_args)

    if not hasattr(args, "func"):
        parser.print_help()
        return 1

    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
