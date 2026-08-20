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
    assert "find \"$journal_root\" -type f -path '*/logs/*.jsonl' -print -delete" in cleanup
    assert "        if: always()\n" in audit
