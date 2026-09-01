"""Static contract for the native Python source-suite workflow (#1494)."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "python-tests.yml"

# `conftest.py` is included because it is a tracked module loaded by the
# python/tests group. The three package __init__.py files are collection markers,
# not runnable test modules.
NATIVE_TEST_MODULES = (
    "python/tests/test_public_api.py",
    "python/tests/cpp_lint/conftest.py",
    "python/tests/cpp_lint/test_cache.py",
    "python/tests/cpp_lint/test_clang_query_integration.py",
    "python/tests/cpp_lint/test_listorpath.py",
    "python/tests/cpp_lint/test_runner_no_tools.py",
    "python/tests/cpp_lint/test_toml.py",
    "python/tests/cpp_lint/test_tools_resolution.py",
    "python/tests/cpp_lint/test_types.py",
    "python/tests/cpp_lint/test_validation.py",
    "crates/zccache-watcher/python/tests/test_compat.py",
    "crates/zccache-watcher/python/tests/test_keyboard_interrupt_behavior.py",
    "crates/zccache-fingerprint/python/tests/test_hash.py",
    "crates/zccache-fingerprint/python/tests/test_manager.py",
    "crates/zccache-fingerprint/python/tests/test_result.py",
)
NATIVE_TEST_GROUPS = (
    "python/tests",
    "crates/zccache-watcher/python/tests",
    "crates/zccache-fingerprint/python/tests",
)


def _job(workflow: str, name: str) -> str:
    marker = f"  {name}:\n"
    start = workflow.index(marker)
    following = re.search(
        r"^  [a-z][a-z0-9-]*:\n",
        workflow[start + len(marker) :],
        re.MULTILINE,
    )
    end = start + len(marker) + following.start() if following else len(workflow)
    return workflow[start:end]


def _step(job: str, name: str) -> str:
    marker = f"      - name: {name}\n"
    start = job.index(marker)
    following = job.find("\n      - name:", start + len(marker))
    return job[start : following if following != -1 else len(job)]


def test_native_python_job_builds_every_extension_separately() -> None:
    native = _job(WORKFLOW.read_text(encoding="utf-8"), "native-pytest")
    build = _step(native, "Build native Python test artifacts")

    assert "runs-on: ubuntu-latest" in native
    assert "zackees/setup-soldr@5f1f68dcb8377818413c28ce52214261ae8ff771" in native
    assert "toolchain: 1.95.0" in native
    expected_builds = (
        "soldr cargo build --release -p zccache --features zccache-bin --bin zccache",
        "soldr cargo build --release -p zccache-cli --features python --lib",
        "soldr cargo build --release -p zccache-watcher-py --lib",
        "soldr cargo build --release -p zccache-fingerprint-py --lib",
    )
    assert build.count("soldr cargo build") == len(expected_builds)
    for command in expected_builds:
        assert command in build
    assert 'export PYO3_PYTHON="$(uv python find 3.13)"' in build
    assert "PYO3_PYTHON: python" not in native


def test_native_python_job_stages_extensions_and_prestarts_the_real_binary() -> None:
    native = _job(WORKFLOW.read_text(encoding="utf-8"), "native-pytest")
    stage = _step(native, "Stage native Python extensions")
    start = _step(native, "Start isolated native Python daemon")

    assert "EXT_SUFFIX" in stage
    assert "uv run --no-project --python 3.13 python -c" in stage
    assert 'EXT_SUFFIX="$(python -c' not in stage
    for destination in (
        "python/zccache/_native$EXT_SUFFIX",
        "python/zccache/watcher/_native$EXT_SUFFIX",
        "python/zccache/fingerprint/_native$EXT_SUFFIX",
        "crates/zccache-watcher/python/zccache/watcher/_native$EXT_SUFFIX",
        "crates/zccache-fingerprint/python/zccache/fingerprint/_native$EXT_SUFFIX",
    ):
        assert destination in stage
    assert 'echo "$GITHUB_WORKSPACE/target/release" >> "$GITHUB_PATH"' in stage
    assert "target/release/zccache start" in start
    assert "target/release/zccache status" in start
    assert "seq 1 30" in start
    assert "ZCCACHE_CACHE_DIR" in start
    assert "ZCCACHE_DAEMON_NAMESPACE: native-python-tests" in start


def test_native_python_job_runs_every_tracked_module_without_deselection() -> None:
    native = _job(WORKFLOW.read_text(encoding="utf-8"), "native-pytest")
    pytest = _step(native, "Run native Python suites")

    assert len(NATIVE_TEST_MODULES) == 15
    assert all((ROOT / module).is_file() for module in NATIVE_TEST_MODULES)
    invocations = pytest.split("python -m pytest")
    assert pytest.count("python -m pytest") == 3
    assert len(invocations) == 4, "each source suite must get its own pytest process"
    source_roots = (
        "PYTHONPATH=python",
        "PYTHONPATH=crates/zccache-watcher/python",
        "PYTHONPATH=crates/zccache-fingerprint/python",
    )
    for group, source_root, launch, invocation in zip(
        NATIVE_TEST_GROUPS, source_roots, invocations, invocations[1:]
    ):
        assert group in invocation
        assert f"{source_root} uv run --no-project --python 3.13" in launch
        assert sum(
            line.strip().rstrip("\\").strip() == group
            for line in pytest.splitlines()
        ) == 1
    assert "--with clang-tool-chain-bins" in invocations[0]
    assert "--with clang-tool-chain-bins" not in invocations[1]
    assert "--with clang-tool-chain-bins" not in invocations[2]
    assert all(
        any(module == group or module.startswith(f"{group}/") for group in NATIVE_TEST_GROUPS)
        for module in NATIVE_TEST_MODULES
    )
    assert "uv run --no-project --python 3.13" in pytest
    assert "--deselect" not in pytest
    assert "--ignore" not in pytest
    assert " -k " not in pytest
    assert "--skip" not in pytest
    assert "ZCCACHE_CACHE_DIR" in pytest
    assert "ZCCACHE_DAEMON_NAMESPACE: native-python-tests" in pytest
    assert "PYTHONPATH: ." not in native


def test_native_python_job_always_stops_and_removes_its_isolated_cache() -> None:
    native = _job(WORKFLOW.read_text(encoding="utf-8"), "native-pytest")
    cleanup = _step(native, "Stop isolated native Python daemon")

    assert "if: always()" in cleanup
    assert "target/release/zccache stop || true" in cleanup
    assert 'rm -rf "$ZCCACHE_CACHE_DIR"' in cleanup
    assert "ZCCACHE_CACHE_DIR" in cleanup
    assert "ZCCACHE_DAEMON_NAMESPACE: native-python-tests" in cleanup
