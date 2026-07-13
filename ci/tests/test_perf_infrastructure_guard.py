"""Adversarial tests for soldr abort isolation in perf scenarios."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
from functools import cache


ROOT = Path(__file__).resolve().parents[2]
COMMON_SH = ROOT / "perf" / "lib" / "common.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "perf-rust-cluster.yml"


@cache
def bash_executable() -> str:
    names = ("bash.exe", "bash") if os.name == "nt" else ("bash",)
    candidates = [
        Path(directory) / name
        for directory in os.environ.get("PATH", "").split(os.pathsep)
        if directory
        for name in names
    ]
    resolved = shutil.which("bash")
    if resolved:
        candidates.append(Path(resolved))
    for candidate in candidates:
        if not candidate.is_file():
            continue
        probe = subprocess.run(
            [str(candidate), "-c", "uname -s"],
            check=False,
            capture_output=True,
            text=True,
        )
        system = probe.stdout.strip()
        if probe.returncode == 0 and (
            os.name != "nt" or system.startswith(("MINGW", "MSYS", "CYGWIN"))
        ):
            return str(candidate)
    raise AssertionError("a working Bash executable is required")


def run_guard(
    tmp_path: Path,
    command: str,
    *,
    record: str | None = None,
    enforce: bool = False,
    expected_status: int = 0,
    pre_guard: str = ":",
) -> dict:
    tmp_path.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    if record is not None:
        env["FAKE_ABORT_RECORD"] = record
    script = r'''
set -euo pipefail
. "$1"
cache="$PWD/cache"
evidence="$PWD/soldr-aborts-test.jsonl"
mkdir -p "$cache/logs"
measure::infrastructure_guard_init
eval "$4"
fake_soldr() {
    eval "$FAKE_COMMAND"
}
export FAKE_COMMAND="$2"
if measure::run_guarded_soldr_command "$cache" "$evidence" "fake compile" fake_soldr; then
    command_status=0
else
    command_status=$?
fi
if (( command_status != 0 )); then
    measure::emit_infrastructure_failure_json test "$command_status"
else
    measure::emit_summary_json test \
        "infrastructure_valid=${_MEASURE_INFRASTRUCTURE_VALID}" \
        "invalid_reasons=json:${_MEASURE_INVALID_REASONS_JSON}" \
        "soldr_abort_count=${_MEASURE_SOLDR_ABORT_COUNT}" \
        "soldr_timeout_count=${_MEASURE_SOLDR_TIMEOUT_COUNT}" \
        "soldr_no_cache_retry_count=${_MEASURE_SOLDR_NO_CACHE_RETRY_COUNT}" \
        "soldr_abort_evidence=json:${_MEASURE_ABORT_EVIDENCE_JSON}"
fi
final_status="$command_status"
if [[ "$3" == true ]]; then
    if measure::fail_if_infrastructure_invalid; then
        guard_status=0
    else
        guard_status=$?
    fi
    if (( final_status == 0 )); then
        final_status="$guard_status"
    fi
fi
exit "$final_status"
'''
    completed = subprocess.run(
        [
            bash_executable(),
            "-c",
            script,
            "guard-test",
            COMMON_SH.as_posix(),
            command,
            str(enforce).lower(),
            pre_guard,
        ],
        cwd=tmp_path,
        env=env,
        check=False,
        text=True,
        capture_output=True,
    )
    assert completed.returncode == expected_status, completed.stderr
    return json.loads(completed.stdout.splitlines()[-1])


def test_stale_abort_record_is_ignored(tmp_path: Path) -> None:
    log = tmp_path / "cache" / "logs" / "cargo-aborts.jsonl"
    log.parent.mkdir(parents=True)
    log.write_text(
        '{"event":"cargo_abort","timeout":true,"auto_retry_planned":true}\n',
        encoding="utf-8",
    )

    result = run_guard(tmp_path, ":")

    assert result["infrastructure_valid"] is True
    assert result["invalid_reasons"] == []
    assert result["soldr_abort_count"] == 0
    assert (tmp_path / "soldr-aborts-test.jsonl").read_text(encoding="utf-8") == ""


def test_new_timeout_and_retry_record_invalidates_sample(tmp_path: Path) -> None:
    record = {
        "event": "cargo_abort",
        "timeout": True,
        "auto_retry_planned": True,
        "recovery": {"retry_without_cache": {"argv": ["cargo", "build"]}},
    }
    result = run_guard(
        tmp_path,
        "printf '%s\\n' \"$FAKE_ABORT_RECORD\" >> \"$cache/logs/cargo-aborts.jsonl\"",
        record=json.dumps(record, separators=(",", ":")),
    )

    assert result["infrastructure_valid"] is False
    assert result["soldr_abort_count"] == 1
    assert result["soldr_timeout_count"] == 1
    assert result["soldr_no_cache_retry_count"] == 1
    assert result["soldr_abort_evidence"] == ["soldr-aborts-test.jsonl"]
    assert "fake compile" in result["invalid_reasons"][0]
    evidence = (tmp_path / "soldr-aborts-test.jsonl").read_text(encoding="utf-8")
    assert json.loads(evidence) == record

    enforced = run_guard(
        tmp_path / "enforced",
        "printf '%s\\n' \"$FAKE_ABORT_RECORD\" >> \"$cache/logs/cargo-aborts.jsonl\"",
        record=json.dumps(record, separators=(",", ":")),
        enforce=True,
        expected_status=1,
    )
    assert enforced["infrastructure_valid"] is False


def test_malformed_new_record_fails_closed(tmp_path: Path) -> None:
    result = run_guard(
        tmp_path,
        "printf '%s' '{\"event\":\"cargo_abort\"' >> \"$cache/logs/cargo-aborts.jsonl\"",
    )

    assert result["infrastructure_valid"] is False
    assert result["soldr_abort_count"] == 0
    assert "malformed or partial" in result["invalid_reasons"][0]


def test_non_timeout_process_abort_invalidates_sample(tmp_path: Path) -> None:
    record = {
        "event": "cargo_abort",
        "timeout": False,
        "auto_retry_planned": False,
        "returncode": 137,
    }
    result = run_guard(
        tmp_path,
        "printf '%s\\n' \"$FAKE_ABORT_RECORD\" >> \"$cache/logs/cargo-aborts.jsonl\"; return 137",
        record=json.dumps(record, separators=(",", ":")),
        expected_status=137,
    )

    assert result["infrastructure_valid"] is False
    assert result["soldr_abort_count"] == 1
    assert result["soldr_timeout_count"] == 0
    assert result["soldr_no_cache_retry_count"] == 0
    assert result["guarded_command_status"] == 137


def test_nonzero_command_without_abort_record_emits_invalid_result(
    tmp_path: Path,
) -> None:
    result = run_guard(tmp_path, "return 42", expected_status=42)

    assert result["infrastructure_valid"] is False
    assert result["soldr_abort_count"] == 0
    assert result["guarded_command_status"] == 42
    assert any("exited with status 42" in reason for reason in result["invalid_reasons"])


def test_capture_failure_preserves_original_command_status(tmp_path: Path) -> None:
    tmp_path.mkdir(parents=True, exist_ok=True)
    (tmp_path / "soldr-aborts-test.jsonl").mkdir()

    result = run_guard(tmp_path, "return 37", expected_status=37)

    assert result["infrastructure_valid"] is False
    assert result["guarded_command_status"] == 37
    assert any("failed to capture" in reason for reason in result["invalid_reasons"])


def test_capture_failure_after_success_fails_and_evaluator_rejects(
    tmp_path: Path,
) -> None:
    tmp_path.mkdir(parents=True, exist_ok=True)
    (tmp_path / "soldr-aborts-test.jsonl").mkdir()

    result = run_guard(tmp_path, ":", expected_status=1)

    assert result["infrastructure_valid"] is False
    assert result["guarded_command_status"] == 1
    assert not evaluator_accepts(
        tmp_path / "evaluator", result, create_evidence=False
    )


def test_abort_log_size_read_failure_fails_closed(tmp_path: Path) -> None:
    result = run_guard(
        tmp_path,
        "measure::soldr_abort_log_bytes() { return 9; }",
        expected_status=1,
    )

    assert result["infrastructure_valid"] is False
    assert result["guarded_command_status"] == 1
    assert any("failed to capture" in reason for reason in result["invalid_reasons"])


def test_initial_abort_log_size_failure_skips_command_and_fails_closed(
    tmp_path: Path,
) -> None:
    result = run_guard(
        tmp_path,
        ': > "$PWD/command-ran"',
        pre_guard="measure::soldr_abort_log_bytes() { return 9; }",
        expected_status=1,
    )

    assert result["infrastructure_valid"] is False
    assert result["guarded_command_status"] == 1
    assert not (tmp_path / "command-ran").exists()
    assert any("initial soldr abort log size" in reason for reason in result["invalid_reasons"])


def test_abort_log_truncation_fails_closed(tmp_path: Path) -> None:
    log = tmp_path / "cache" / "logs" / "cargo-aborts.jsonl"
    log.parent.mkdir(parents=True)
    log.write_text(
        '{"event":"cargo_abort","timeout":false,"auto_retry_planned":false}\n',
        encoding="utf-8",
    )

    result = run_guard(tmp_path, ': > "$cache/logs/cargo-aborts.jsonl"')

    assert result["infrastructure_valid"] is False
    assert "truncated" in result["invalid_reasons"][0]


def test_equal_size_abort_log_rewrite_fails_closed(tmp_path: Path) -> None:
    log = tmp_path / "cache" / "logs" / "cargo-aborts.jsonl"
    log.parent.mkdir(parents=True)
    log.write_bytes(b'{"event":"cargo_abort","timeout":0}\n')

    result = run_guard(
        tmp_path,
        "printf '%s\\n' '{\"event\":\"cargo_abort\",\"timeout\":1}' "
        '> "$cache/logs/cargo-aborts.jsonl"',
    )

    assert result["infrastructure_valid"] is False
    assert "existing prefix changed" in result["invalid_reasons"][0]


def test_abort_in_another_scenario_root_is_ignored(tmp_path: Path) -> None:
    result = run_guard(
        tmp_path,
        "mkdir -p \"$PWD/other-cache/logs\"; "
        "printf '%s\\n' '{\"event\":\"cargo_abort\",\"timeout\":true}' "
        ">> \"$PWD/other-cache/logs/cargo-aborts.jsonl\"",
    )

    assert result["infrastructure_valid"] is True
    assert result["soldr_abort_count"] == 0


def test_diagnostic_words_without_structured_record_are_not_false_positive(
    tmp_path: Path,
) -> None:
    result = run_guard(tmp_path, "echo 'timeout; retry without cache' >&2")

    assert result["infrastructure_valid"] is True
    assert result["soldr_abort_count"] == 0


def test_short_delayed_command_has_no_guard_deadline(tmp_path: Path) -> None:
    result = run_guard(tmp_path, "sleep 0.2")

    assert result["infrastructure_valid"] is True
    assert result["soldr_timeout_count"] == 0


def evaluator_function() -> str:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    marker = "          validate_infrastructure_result() {"
    start = workflow.index(marker)
    end = workflow.index("\n\n          # ---- formatting helpers", start)
    return workflow[start:end]


def run_evaluator(
    tmp_path: Path, payload: object, *, create_evidence: bool = True
) -> subprocess.CompletedProcess[str]:
    result = tmp_path / "result.json"
    result.parent.mkdir(parents=True, exist_ok=True)
    result.write_text(json.dumps(payload), encoding="utf-8")
    script = evaluator_function() + r'''
if [[ "$2" == true ]]; then
    while IFS= read -r evidence; do
        : > "$evidence"
    done < <(jq -r '.soldr_abort_evidence[]?' "$1")
fi
validate_infrastructure_result "$1"
'''
    completed = subprocess.run(
        [
            bash_executable(),
            "-c",
            script,
            "evaluator-test",
            "result.json",
            str(create_evidence).lower(),
        ],
        cwd=tmp_path,
        check=False,
        text=True,
        capture_output=True,
    )
    return completed


def evaluator_accepts(
    tmp_path: Path, payload: object, *, create_evidence: bool = True
) -> bool:
    return run_evaluator(
        tmp_path, payload, create_evidence=create_evidence
    ).returncode == 0


def valid_result() -> dict:
    return {
        "infrastructure_valid": True,
        "invalid_reasons": [],
        "soldr_abort_count": 0,
        "soldr_timeout_count": 0,
        "soldr_no_cache_retry_count": 0,
        "soldr_abort_evidence": ["soldr-aborts-cold.jsonl"],
    }


def test_evaluator_rejects_every_missing_guard_field(tmp_path: Path) -> None:
    valid = valid_result()
    completed = run_evaluator(tmp_path / "valid", valid)
    assert completed.returncode == 0, completed.stdout + completed.stderr

    for field in tuple(valid):
        incomplete = valid.copy()
        del incomplete[field]
        assert not evaluator_accepts(tmp_path / field, incomplete), field


def test_evaluator_rejects_malformed_or_inconsistent_fields(tmp_path: Path) -> None:
    malformed = valid_result()
    malformed["soldr_abort_count"] = "0"
    assert not evaluator_accepts(tmp_path / "malformed", malformed)

    inconsistent = valid_result()
    inconsistent["soldr_timeout_count"] = 1
    assert not evaluator_accepts(tmp_path / "inconsistent", inconsistent)

    unexplained_invalid = valid_result()
    unexplained_invalid["infrastructure_valid"] = False
    assert not evaluator_accepts(tmp_path / "unexplained", unexplained_invalid)


def test_evaluator_rejects_schema_valid_contamination_and_missing_evidence(
    tmp_path: Path,
) -> None:
    contaminated = valid_result()
    contaminated.update(
        infrastructure_valid=False,
        invalid_reasons=["warm build: soldr timeout"],
        soldr_abort_count=1,
        soldr_timeout_count=1,
    )
    assert not evaluator_accepts(tmp_path / "contaminated", contaminated)
    assert not evaluator_accepts(
        tmp_path / "missing-evidence", valid_result(), create_evidence=False
    )
