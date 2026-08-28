from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def _step_block(workflow: str, name: str, next_name: str | None = None) -> str:
    block = workflow.split(f"      - name: {name}\n", 1)[1]
    if next_name is not None:
        block = block.split(f"      - name: {next_name}\n", 1)[0]
    return block


def test_hosted_daemon_core_and_workspace_test_compiles_are_serialized() -> None:
    """Every hosted lib-test/full-workspace compile names both admission limits."""
    required_steps = {
        "integration.yml": (
            (
                "Build integration test binaries",
                "Stop setup-soldr builder cache before tests",
                "soldr cargo test --workspace",
            ),
            (
                "Test (full workspace)",
                "Remove build-harness journals before strict audit",
                "soldr cargo test --workspace",
            ),
            (
                "Test ignored integration and stress suite",
                "Remove ignored-suite harness journals before strict audit",
                "soldr cargo test --workspace",
            ),
        ),
        "ci-check.yml": (("Test", "Stop isolated test cache", "soldr cargo test --workspace --lib --bins"),),
        "coverage.yml": (("Generate coverage", "Stop isolated coverage cache", "soldr cargo llvm-cov --workspace --lib --bins"),),
        "fs-matrix.yml": (
            (
                "Run capability and behavior matrix",
                "ReFS cluster-rounding acceptance",
                "soldr cargo test -p zccache-daemon-core",
            ),
            (
                "ReFS cluster-rounding acceptance",
                None,
                "soldr cargo test -p zccache-daemon-core",
            ),
            (
                "Run required >4 GiB COW acceptance",
                None,
                "soldr cargo test -p zccache-daemon-core",
            ),
        ),
        "perf-guard.yml": (
            (
                "Run COW materialization budget",
                "Build perf benchmark binary",
                "soldr cargo test -p zccache-daemon-core",
            ),
        ),
    }

    for workflow_name, steps in required_steps.items():
        workflow = (ROOT / ".github" / "workflows" / workflow_name).read_text(encoding="utf-8")
        for name, next_name, command in steps:
            step = _step_block(workflow, name, next_name)
            environment = step.split("        run:", 1)[0]
            assert command in step
            assert 'CARGO_BUILD_JOBS: "1"' in environment
            assert 'SOLDR_JOBS: "1"' in environment
