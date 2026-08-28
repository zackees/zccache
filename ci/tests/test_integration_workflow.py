from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "integration.yml"


def _step_block(workflow: str, name: str, next_name: str | None = None) -> str:
    block = workflow.split(f"      - name: {name}\n", 1)[1]
    if next_name is not None:
        block = block.split(f"      - name: {next_name}\n", 1)[0]
    return block


def test_build_harness_journal_cleanup_runs_after_test_failure() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    cleanup = _step_block(
        workflow,
        "Remove build-harness journals before strict audit",
        "Wrapper daemon-unavailable contract (exit 125)",
    )
    audit = _step_block(
        workflow,
        "Audit isolated integration cache",
    )

    assert "        if: always()\n" in cleanup
    assert "ci/clear_runtime_telemetry.py" in cleanup
    assert '--cache-root "$SOLDR_CACHE_DIR/cache/zccache"' in cleanup
    assert "        if: always()\n" in audit
    assert "astral-sh/setup-uv@v6" in workflow


def test_wrapper_contract_telemetry_is_cleared_before_the_strict_audit() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    cleanup = _step_block(
        workflow,
        "Remove wrapper-contract telemetry before strict audit",
        "Strict artifact-layout validation",
    )

    assert "        if: always()\n" in cleanup
    assert "ci/clear_runtime_telemetry.py" in cleanup
    assert '--cache-root "$SOLDR_CACHE_DIR/cache/zccache"' in cleanup


def test_ignored_suite_cleanup_uses_the_same_full_telemetry_helper() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    cleanup = _step_block(
        workflow,
        "Remove ignored-suite harness journals before strict audit",
        "Stop isolated test cache",
    )

    assert "if: always()" in cleanup
    assert "ci/clear_runtime_telemetry.py" in cleanup
    assert '--cache-root "$SOLDR_CACHE_DIR/cache/zccache"' in cleanup


def test_workspace_test_phases_bound_compile_and_runtime_concurrency() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    full_suite = _step_block(
        workflow,
        "Test (full workspace)",
        "Remove build-harness journals before strict audit",
    )
    ignored_suite = _step_block(
        workflow,
        "Test ignored integration and stress suite",
        "Remove ignored-suite harness journals before strict audit",
    )

    for suite in (full_suite, ignored_suite):
        assert 'CARGO_BUILD_JOBS: "1"' in suite
        assert 'SOLDR_JOBS: "1"' in suite
        assert "--test-threads=1" in suite
