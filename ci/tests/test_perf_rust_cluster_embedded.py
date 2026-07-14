"""Regression tests for the authoritative local Docker perf harness."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "perf-rust-cluster.yml"
LOCAL_ENTRYPOINT = ROOT / "ci" / "docker" / "perf_entrypoint.sh"
LOCAL_RUNNER = ROOT / "ci" / "docker" / "runner.Dockerfile"
LOCAL_ORCHESTRATOR = ROOT / "ci" / "perf_local.py"
COMMON_SH = ROOT / "perf" / "lib" / "common.sh"
SNAPSHOT_SCENARIOS = (
    ROOT / "perf" / "scenarios" / "cold-tar-untar-warm" / "run.sh",
    ROOT / "perf" / "scenarios" / "restore-no-clean-warm" / "run.sh",
)
ROLLOUT_SCENARIOS = SNAPSHOT_SCENARIOS + (
    ROOT / "perf" / "scenarios" / "worktree-share" / "run.sh",
    ROOT / "perf" / "scenarios" / "touch-no-change" / "run.sh",
)


def test_hosted_perf_cluster_is_removed() -> None:
    assert not WORKFLOW.exists()


def test_perf_local_does_not_use_removed_runtime_zccache_pinning() -> None:
    local_harness = "\n".join(path.read_text(encoding="utf-8") for path in (LOCAL_ENTRYPOINT, LOCAL_RUNNER, LOCAL_ORCHESTRATOR))

    assert "soldr update-zccache" not in local_harness
    assert "/zccache-bin" not in local_harness
    assert '"--skip-soldr-build",' not in LOCAL_ORCHESTRATOR.read_text(encoding="utf-8")


def test_perf_local_lists_daemon_runtime_without_copying_special_files() -> None:
    entrypoint = LOCAL_ENTRYPOINT.read_text(encoding="utf-8")

    assert "warm-daemon-files.txt" in entrypoint
    assert 'copy_if_exists "${scenario_root}/cache-warm/cache/soldr-daemon"' not in entrypoint


def test_perf_local_runner_installs_scenario_dependencies() -> None:
    runner = LOCAL_RUNNER.read_text(encoding="utf-8")

    assert "        git \\\n" in runner
    assert "        procps \\\n" in runner


def test_perf_local_final_rollout_matrix_and_gates_are_required() -> None:
    orchestrator = LOCAL_ORCHESTRATOR.read_text(encoding="utf-8")

    assert '"--matrix"' in orchestrator
    assert 'VALID_FIXTURES = ("medium", "sqlite-link")' in orchestrator
    for scenario in (
        "cold-tar-untar-warm",
        "restore-no-clean-warm",
        "worktree-share",
        "touch-no-change",
    ):
        assert scenario in orchestrator
    for required in (
        "LOCAL_MAX_STAGED_OVERHEAD_MS",
        "LOCAL_MAX_MATERIALIZATION_COPIED_BYTES",
        "publication_success",
        "materialize_reflink",
        "materialize_hardlink_shared",
        "materialize_copy",
        "salvage_attempt",
        "materialize_failure",
        "warm_misses",
        "restore warm build had cache misses",
    ):
        assert required in orchestrator


def test_perf_local_fails_closed_on_soldr_abort_contamination() -> None:
    orchestrator = LOCAL_ORCHESTRATOR.read_text(encoding="utf-8")

    for required in (
        "infrastructure_valid",
        "invalid_reasons",
        "soldr_abort_count",
        "soldr_timeout_count",
        "soldr_no_cache_retry_count",
        "soldr_abort_evidence",
        "missing or malformed infrastructure-validity fields",
    ):
        assert required in orchestrator

    common = COMMON_SH.read_text(encoding="utf-8")
    assert "measure::run_guarded_soldr_command()" in common
    assert "cargo-aborts.jsonl" in common
    assert '$5 == "soldr" || $5 == "zccache-daemon"' in common

    for scenario in ROLLOUT_SCENARIOS:
        script = scenario.read_text(encoding="utf-8")
        assert script.count("measure::run_guarded_soldr_command") == 2
        assert script.count("soldr cargo build --release") == script.count("measure::run_guarded_soldr_command")
        assert script.count("measure::emit_infrastructure_failure_json") == 2
        assert script.count('|| echo "failed to emit infrastructure failure JSON"') == 2
        assert '"infrastructure_valid=${_MEASURE_INFRASTRUCTURE_VALID}"' in script
        assert '"invalid_reasons=json:${_MEASURE_INVALID_REASONS_JSON}"' in script
        assert "measure::fail_if_infrastructure_invalid" in script


def test_windows_rss_poller_does_not_create_a_lockable_script() -> None:
    common = COMMON_SH.read_text(encoding="utf-8")
    windows_poller = common.split("MINGW*|MSYS*|CYGWIN*)", 1)[1].split("        *)", 1)[0]

    assert "powershell.exe" in windows_poller
    assert "-Command -" in windows_poller
    assert ".poll.ps1" not in windows_poller
    assert "_MEASURE_RSS_PS1" not in common


def test_snapshot_scenarios_stop_the_sqlite_owner_before_save() -> None:
    common = COMMON_SH.read_text(encoding="utf-8")

    assert "measure::quiesce_cache_for_snapshot()" in common
    assert "soldr cache flush --json" in common
    assert "soldr cache shutdown" in common
    assert "soldr daemon stop" in common
    assert "measure::_pid_is_alive" in common
    assert "Get-Process -Id" in common
    assert "kill -0" in common
    assert "soldr-daemon did not exit" in common

    for scenario in SNAPSHOT_SCENARIOS:
        script = scenario.read_text(encoding="utf-8")
        before_save = script.split("\nsoldr save \\\n", 1)[0]
        save_command = script.split("\nsoldr save \\\n", 1)[1].split("\ntar_bytes=", 1)[0]
        assert "measure::quiesce_cache_for_snapshot" in before_save
        assert "    --ci \\\n" in save_command
        assert '"archive_mode=soldr-save-load-ci"' in script
        assert "soldr cache flush --json >/dev/null 2>&1 || true" not in script
