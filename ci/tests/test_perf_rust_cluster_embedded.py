"""Regression tests for the embedded-zccache perf-cluster bootstrap."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "perf-rust-cluster.yml"


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
        'git -C soldr-src/_vender/zccache checkout --detach "$GITHUB_SHA"'
        in pin_step
    )
    assert 'rev-parse HEAD)" = "$GITHUB_SHA"' in pin_step
    assert workflow.index(pin_step_name) < workflow.index("Build soldr (release)")
    assert "${{ github.sha }}" in workflow.split("key: soldr-", 1)[1].splitlines()[0]


def test_perf_cluster_does_not_use_removed_runtime_zccache_pinning() -> None:
    workflow = workflow_text()

    assert "soldr update-zccache" not in workflow


def test_perf_cluster_pins_cache_action_to_an_immutable_commit() -> None:
    workflow = workflow_text()
    expected_sha = "0057852bfaa89a56745cba8c7296529d2fc39830"
    cache_refs = [
        line.split("actions/cache@", 1)[1].split()[0]
        for line in workflow.splitlines()
        if "uses: actions/cache@" in line
    ]

    assert cache_refs == [expected_sha, expected_sha]
