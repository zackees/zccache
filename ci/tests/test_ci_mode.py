"""The CI selector must fail closed before an expensive or release job starts."""

from pathlib import Path

import yaml

from ci.ci_mode import select_mode, selected_workflows


def test_routine_and_label_modes():
    assert select_mode("pull_request", []) == "minimal"
    assert select_mode("push", []) == "minimal"
    assert select_mode("pull_request", ["ci-test"]) == "extended"
    assert select_mode("pull_request", ["ci-test", "ci-full"]) == "full"
    assert select_mode("schedule", []) == "full"
    assert select_mode("workflow_dispatch", []) == "full"


def test_unknown_ci_label_fails_closed():
    try:
        select_mode("pull_request", ["ci-ful"])
    except ValueError:
        pass
    else:
        raise AssertionError("unknown CI labels must fail")


def test_release_requires_exact_sha():
    assert select_mode("workflow_dispatch", [], "full", "abc", "abc") == "full"
    try:
        select_mode("workflow_dispatch", [], "full", "abc", "def")
    except ValueError:
        pass
    else:
        raise AssertionError("a different candidate SHA must fail")


def test_full_covers_every_registered_workflow():
    full = selected_workflows("full")
    assert selected_workflows("minimal") < selected_workflows("extended") < full
    assert {
        "ci.yml",
        "ci-linux.yml",
        "ci-macos.yml",
        "ci-windows.yml",
        "fs-matrix.yml",
        "integration.yml",
    } <= full


def test_registered_workflows_use_the_selector():
    root = Path(__file__).resolve().parents[2]
    for name in selected_workflows("full"):
        workflow = yaml.safe_load((root / ".github/workflows" / name).read_text())
        assert (
            workflow["jobs"]["select"]["uses"] == "./.github/workflows/ci-select.yml"
        ), name


def test_no_independent_pr_or_main_workflow_is_forgotten():
    root = Path(__file__).resolve().parents[2] / ".github/workflows"
    independently_triggered = set()
    for path in root.glob("*.yml"):
        workflow = yaml.safe_load(path.read_text())
        # PyYAML treats YAML 1.1's `on` as a boolean key.
        events = workflow.get("on", workflow.get(True, {})) or {}
        if {"push", "pull_request"} & events.keys():
            independently_triggered.add(path.name)
    assert independently_triggered - selected_workflows("full") == {"release-auto.yml"}


def test_clippy_and_coverage_filter_main_docs_but_allow_full_docs_prs():
    root = Path(__file__).resolve().parents[2] / ".github/workflows"
    for name in ("clippy.yml", "coverage.yml"):
        workflow = yaml.safe_load((root / name).read_text())
        events = workflow.get("on", workflow.get(True, {}))
        assert "**/*.md" in events["push"]["paths-ignore"]
        assert "paths-ignore" not in events["pull_request"]


def test_full_only_jobs_cannot_run_without_full_selection():
    root = Path(__file__).resolve().parents[2] / ".github/workflows"
    for name in selected_workflows("full") - selected_workflows("minimal"):
        jobs = yaml.safe_load((root / name).read_text())["jobs"]
        for job_name, job in jobs.items():
            if job_name == "select":
                continue
            needs = job.get("needs", [])
            needs = [needs] if isinstance(needs, str) else needs
            condition = str(job.get("if", ""))
            if "select" in needs:
                assert (
                    "needs.select.outputs.selected == 'true'" in condition
                    or "needs.select.outputs.mode == 'full'" in condition
                ), (name, job_name)
            else:
                assert needs, (name, job_name)
                assert "always()" not in condition, (name, job_name)
                assert all(dependency in jobs for dependency in needs), (
                    name,
                    job_name,
                )


def test_apple_hosts_are_native_and_full_only():
    root = Path(__file__).resolve().parents[2] / ".github/workflows"
    macos = yaml.safe_load((root / "ci-macos.yml").read_text())["jobs"]["macos"]
    assert macos["if"] == "needs.select.outputs.selected == 'true'"
    assert set(macos["strategy"]["matrix"]["os"]) == {
        "macos-15-intel",
        "macos-15",
    }

    fs = yaml.safe_load((root / "fs-matrix.yml").read_text())["jobs"]["matrix"]
    os_expression = fs["strategy"]["matrix"]["os"]
    assert "github.event_name == 'schedule'" in os_expression
    assert "macos-15-intel" in os_expression
    assert "macos-15" in os_expression
    assert "windows-latest" in os_expression
    assert "ubuntu-latest" in os_expression

    action = yaml.safe_load((root / "test-action.yml").read_text())["jobs"]
    action_hosts = {
        item["target"]: item["os"]
        for item in action["test-action"]["strategy"]["matrix"]["include"]
    }
    assert action_hosts["x86_64-apple-darwin"] == "macos-15-intel"
    assert action_hosts["aarch64-apple-darwin"] == "macos-15"

    wrapper_hosts = yaml.safe_load((root / "wrapper-e2e.yml").read_text())["jobs"][
        "wrapper-e2e"
    ]["strategy"]["matrix"]["os"]
    assert "needs.select.outputs.mode == 'full'" in wrapper_hosts
    assert '"macos-15-intel","macos-15"' in wrapper_hosts
    assert "'[\"ubuntu-latest\",\"windows-latest\"]'" in wrapper_hosts

    release = yaml.safe_load((root / "release-auto.yml").read_text())["jobs"]
    intel_wheel = release["test-wheels-ungated"]["strategy"]["matrix"]["include"]
    assert any(
        item["wheel_plat"] == "macosx_10_12_x86_64"
        and item["os"] == "macos-15-intel"
        for item in intel_wheel
    )
