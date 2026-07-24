from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "broker-stress.yml"


def test_broker_stress_workflow_requires_and_audits_the_daemon() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert "SOLDR_DAEMON_REQUIRED: \"1\"" in workflow
    assert "for worker in $(seq 1 4)" in workflow
    assert "for attempt in $(seq 1 12)" in workflow
    assert "compile_daemon_fallback|no_cache_retry|timeout|abort" in workflow
    assert 'cache shutdown --archive-logs "$RUNNER_TEMP/soldr-broker-evidence"' in workflow
    assert "audit-logs \"$SOLDR_CACHE_DIR/cache/zccache\" --context integration" in workflow
    assert "schedule:" in workflow
    assert "workflow_dispatch:" in workflow
