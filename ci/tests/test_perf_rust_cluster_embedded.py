"""Regression tests for the embedded-zccache perf-cluster bootstrap."""

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


def workflow_text() -> str:
    return WORKFLOW.read_text(encoding="utf-8")


def test_perf_cluster_builds_soldr_with_the_zccache_commit_under_test() -> None:
    workflow = workflow_text()
    pin_step_name = "Pin zccache source into soldr build"
    pin_step = workflow.split(f"- name: {pin_step_name}", 1)[1].split(
        "\n      - name:", 1
    )[0]

    assert 'git -C soldr-src/_vender/zccache fetch origin "$GITHUB_SHA"' in pin_step
    assert (
        'git -C soldr-src/_vender/zccache checkout --detach "$GITHUB_SHA"' in pin_step
    )
    assert 'rev-parse HEAD)" = "$GITHUB_SHA"' in pin_step
    assert workflow.index(pin_step_name) < workflow.index("Build soldr (release)")
    assert "${{ github.sha }}" in workflow.split("key: soldr-", 1)[1].splitlines()[0]


def test_perf_cluster_does_not_use_removed_runtime_zccache_pinning() -> None:
    workflow = workflow_text()

    assert "soldr update-zccache" not in workflow


def test_perf_local_does_not_use_removed_runtime_zccache_pinning() -> None:
    local_harness = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (LOCAL_ENTRYPOINT, LOCAL_RUNNER, LOCAL_ORCHESTRATOR)
    )

    assert "soldr update-zccache" not in local_harness
    assert "/zccache-bin" not in local_harness
    assert '"--skip-soldr-build",' not in LOCAL_ORCHESTRATOR.read_text(encoding="utf-8")


def test_perf_local_lists_daemon_runtime_without_copying_special_files() -> None:
    entrypoint = LOCAL_ENTRYPOINT.read_text(encoding="utf-8")

    assert "warm-daemon-files.txt" in entrypoint
    assert (
        'copy_if_exists "${scenario_root}/cache-warm/cache/soldr-daemon"'
        not in entrypoint
    )


def test_perf_local_runner_installs_worktree_scenario_dependencies() -> None:
    runner = LOCAL_RUNNER.read_text(encoding="utf-8")

    assert "        git \\\n" in runner


def test_perf_cluster_pins_cache_action_to_an_immutable_commit() -> None:
    workflow = workflow_text()
    expected_sha = "0057852bfaa89a56745cba8c7296529d2fc39830"
    cache_refs = [
        line.split("actions/cache@", 1)[1].split()[0]
        for line in workflow.splitlines()
        if "uses: actions/cache@" in line
    ]

    assert cache_refs == [expected_sha, expected_sha]


def test_perf_cluster_final_rollout_matrix_and_defaults_are_required() -> None:
    workflow = workflow_text()

    assert workflow.count("default: all") >= 3
    assert "platform: [linux, mac-arm, win]" in workflow
    for runner in ("ubuntu-24.04", "macos-14", "windows-2025"):
        assert workflow.count(f"runs_on: {runner}") >= 3
    assert "fixture: [medium, sqlite-link]" in workflow
    assert 'max_warm_ms_worktree: "6000"' in workflow
    assert 'max_warm_ms_touch: "5000"' in workflow
    assert 'min_speedup: "0.05"' not in workflow
    assert workflow.count('min_speedup: "3.0"') == 1
    assert 'min_speedup: "1.3"' in workflow
    assert 'min_speedup_sqlite_worktree: "1.25"' in workflow
    assert workflow.count('max_warm_ms_restore: "5000"') == 1
    assert 'max_warm_ms_worktree: "30000"' in workflow
    assert 'max_warm_ms_sqlite_worktree: "35000"' in workflow
    assert workflow.count('max_warm_ms_touch: "30000"') == 1
    assert (
        "cold-tar-untar-warm|restore-no-clean-warm|worktree-share|touch-no-change) "
        'echo "fail"'
    ) in workflow


def test_perf_cluster_gates_staged_metrics_and_restore_noop() -> None:
    workflow = workflow_text()

    for required in (
        "MAX_STAGED_OVERHEAD_MS",
        "MAX_MATERIALIZATION_COPIED_BYTES",
        "publication_success",
        "materialize_reflink",
        "materialize_hardlink_shared",
        "materialize_copy",
        "salvage_attempt",
        "materialize_failure",
        "warm_compilations",
        "warm_misses",
        "restore warm build had cache misses",
    ):
        assert required in workflow


def test_perf_cluster_normalizes_windows_temp_for_git_bash_tools() -> None:
    workflow = workflow_text()
    run_step = workflow.split("- name: Run selected scenarios", 1)[1].split(
        "\n      - name:", 1
    )[0]

    assert 'runner_temp="$(cygpath -u "${RUNNER_TEMP}")"' in run_step
    assert 'out_root="${runner_temp}/perf-${M_PLATFORM}-${M_FIXTURE}"' in run_step
    assert 'artifact_root="$(cygpath -w "${out_root}")"' in run_step
    assert 'echo "out_root=${artifact_root}" >> "$GITHUB_OUTPUT"' in run_step
    assert "SOLDR_CARGO_WAIT_TIMEOUT_SECS" not in run_step


def test_perf_cluster_fails_closed_on_soldr_abort_contamination() -> None:
    workflow = workflow_text()

    for required in (
        "infrastructure_valid",
        "invalid_reasons",
        "soldr_abort_count",
        "soldr_timeout_count",
        "soldr_no_cache_retry_count",
        "soldr_abort_evidence",
        "INFRA-INVALID",
        "missing or malformed infrastructure-validity fields",
        "soldr-aborts-*.jsonl",
    ):
        assert required in workflow

    common = COMMON_SH.read_text(encoding="utf-8")
    assert "measure::run_guarded_soldr_command()" in common
    assert "cargo-aborts.jsonl" in common

    for scenario in ROLLOUT_SCENARIOS:
        script = scenario.read_text(encoding="utf-8")
        assert script.count("measure::run_guarded_soldr_command") == 2
        assert script.count("soldr cargo build --release") == script.count(
            "measure::run_guarded_soldr_command"
        )
        assert script.count("measure::emit_infrastructure_failure_json") == 2
        assert script.count('|| echo "failed to emit infrastructure failure JSON"') == 2
        assert '"infrastructure_valid=${_MEASURE_INFRASTRUCTURE_VALID}"' in script
        assert '"invalid_reasons=json:${_MEASURE_INVALID_REASONS_JSON}"' in script
        assert "measure::fail_if_infrastructure_invalid" in script


def test_windows_rss_poller_does_not_create_a_lockable_script() -> None:
    common = COMMON_SH.read_text(encoding="utf-8")
    windows_poller = common.split("MINGW*|MSYS*|CYGWIN*)", 1)[1].split(
        "        *)", 1
    )[0]

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
