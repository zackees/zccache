"""Integration retains its isolated compile store without sharing Test's key."""

from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]


def test_integration_publishes_its_own_store_after_all_consumers() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/integration.yml").read_text(encoding="utf-8")
    )
    steps = workflow["jobs"]["integration"]["steps"]
    setup = next(step for step in steps if step.get("uses") == "zackees/setup-soldr@v0")
    assert setup.get("id") == "soldr"
    assert setup["with"]["cache-key-suffix"] == "integration"
    assert setup["with"]["prebuild-deps"] == "none"
    assert setup["with"]["ci-tests"] is True
    assert setup["with"]["cargo-registry-cache"] is True
    assert not any(
        step.get("uses", "").startswith("zackees/setup-soldr/cook@") for step in steps
    )
    assert setup["with"]["job-status"] == "${{ job.status }}"
    assert setup["with"]["save-cache"] == "auto"
    assert "github.event_name == 'push'" in setup["with"]["save-cache-remote"]
    isolated = setup["with"]["seed-isolated-build-cache"]
    publications = [
        index
        for index, step in enumerate(steps)
        if step.get("uses") == "./.github/actions/publish-isolated-build-cache"
    ]
    assert len(publications) == 1
    index = publications[0]
    publish = steps[index]
    assert publish["if"] == "always()"
    assert publish["with"]["isolated-soldr-cache-dir"] == isolated
    assert (
        publish["with"]["build-cache-path"]
        == "${{ steps.soldr.outputs.build-cache-path }}"
    )
    consumers = [
        position
        for position, step in enumerate(steps)
        if step.get("env", {}).get("SOLDR_CACHE_DIR") == isolated
    ]
    assert index > max(consumers)
    assert any(
        "soldr cache shutdown" in steps[position].get("run", "")
        for position in consumers
    )
    assert steps[index - 1]["name"] == "Audit isolated integration cache"
