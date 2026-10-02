"""The Linux x64 Test job keeps its own warm compile store (zccache#1867).

`ci-check.yml`'s Test job compiles under an isolated `SOLDR_CACHE_DIR` that
setup-soldr seeds from the build-cache it restored. Before #1867 two things
kept every workspace compile a miss on warm runs (0 HIT / 47 MISS, hosted and
under `bosn ci`):

* the job restored the build-cache key shared by about ten other jobs, whose
  first writer ("Miss overhead budget") never compiles the test profile; and
* setup-soldr's post step saves only its own store, so the isolated store the
  job actually compiled into was thrown away at job end.

These tests hold the fix: the Linux x64 leg has its own build-cache key, and
the isolated store is published back into setup-soldr's build-cache path after
its daemon stops. The publish script itself is executed, not pattern-matched.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "ci-check.yml"
PUBLISH_ACTION = "./.github/actions/publish-isolated-build-cache"
ACTION_FILE = ROOT / PUBLISH_ACTION / "action.yml"
SETUP_SOLDR = "zackees/setup-soldr@"
LINUX_X64 = "inputs.os == 'ubuntu-latest'"


@dataclass(frozen=True)
class Step:
    index: int
    name: str
    uses: str
    if_: str
    run: str
    with_: dict[str, str]
    env: dict[str, str]
    id: str

    @classmethod
    def parse(cls, index: int, raw: dict) -> "Step":
        return cls(
            index=index,
            name=str(raw.get("name", "")),
            uses=str(raw.get("uses", "")),
            if_=str(raw.get("if", "")),
            run=str(raw.get("run", "")),
            with_={str(k): str(v) for k, v in (raw.get("with") or {}).items()},
            env={str(k): str(v) for k, v in (raw.get("env") or {}).items()},
            id=str(raw.get("id", "")),
        )


def _test_job_steps() -> list[Step]:
    doc = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
    raw_steps = doc["jobs"]["test"]["steps"]
    return [Step.parse(i, raw) for i, raw in enumerate(raw_steps)]


def _setup_soldr(steps: list[Step]) -> Step:
    found = [s for s in steps if s.uses.startswith(SETUP_SOLDR)]
    assert len(found) == 1, "the Test job has exactly one setup-soldr step"
    return found[0]


def test_linux_x64_test_job_owns_its_build_cache_key() -> None:
    setup = _setup_soldr(_test_job_steps())
    suffix = setup.with_.get("cache-key-suffix", "")
    # A shared key restores whichever job saved first; that store never holds
    # the test-profile compiles this job needs.
    assert LINUX_X64 in suffix and "'test'" in suffix, suffix


def test_isolated_store_is_published_after_its_daemon_stops() -> None:
    steps = _test_job_steps()
    setup = _setup_soldr(steps)
    isolated = setup.with_["seed-isolated-build-cache"]
    assert setup.id, "setup-soldr needs an id so its build-cache-path is addressable"

    publishes = [s for s in steps if s.uses == PUBLISH_ACTION]
    assert len(publishes) == 1, "the isolated store must be published back exactly once"
    publish = publishes[0]
    assert publish.with_["isolated-soldr-cache-dir"] == isolated
    assert (
        publish.with_["build-cache-path"]
        == f"${{{{ steps.{setup.id}.outputs.build-cache-path }}}}"
    )
    assert LINUX_X64 in publish.if_ and "always()" in publish.if_

    users = [s for s in steps if s.env.get("SOLDR_CACHE_DIR") == isolated]
    stop = [s for s in users if "soldr cache shutdown" in s.run]
    assert stop, "a step must stop the isolated daemon so its index is flushed"
    assert publish.index > max(s.index for s in users), (
        "publish only after every step that compiles into the isolated store"
    )


# ---- the publish script, executed --------------------------------------------


def _publish_script() -> str:
    doc = yaml.safe_load(ACTION_FILE.read_text(encoding="utf-8"))
    runs = [s["run"] for s in doc["runs"]["steps"] if "run" in s]
    assert len(runs) == 1
    return runs[0]


BASH = None if os.name == "nt" else shutil.which("bash")
needs_bash = pytest.mark.skipif(BASH is None, reason="needs a POSIX bash")


def _run_publish(isolated: Path, build_cache_path: str) -> subprocess.CompletedProcess:
    env = dict(os.environ)
    env["ISOLATED_SOLDR_CACHE_DIR"] = str(isolated)
    env["BUILD_CACHE_PATH"] = build_cache_path
    return subprocess.run(
        [BASH, "-e", "-c", _publish_script()],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


@needs_bash
def test_publish_replaces_the_build_cache_with_the_isolated_store(tmp_path: Path) -> None:
    isolated = tmp_path / "zccache-self-tests" / "test-ubuntu-latest"
    store = isolated / "cache" / "zccache"
    _write(store / "daemon-state" / "embedded-v1" / "artifacts" / "ab" / "abcd", "rlib")
    _write(store / "daemon-state" / "embedded-v1" / "index.bin", "index-after-build")
    _write(store / "logs" / "archive" / "s1" / "last-session-stats.json", "{}")
    build_cache = tmp_path / "setup-soldr-soldr" / "cache" / "zccache"
    _write(build_cache / "daemon-state" / "embedded-v1" / "index.bin", "restored-index")
    _write(build_cache / "daemon-state" / "embedded-v1" / "artifacts" / "zz" / "stale", "x")

    result = _run_publish(isolated, str(build_cache))

    assert result.returncode == 0, result.stderr
    published = build_cache / "daemon-state" / "embedded-v1"
    assert (published / "artifacts" / "ab" / "abcd").read_text() == "rlib"
    assert (published / "index.bin").read_text() == "index-after-build"
    assert not (published / "artifacts" / "zz" / "stale").exists()
    # Session logs travel too: setup-soldr's new-compile save gate reads
    # them from the build-cache path; its save profile trims them.
    assert (build_cache / "logs" / "archive" / "s1" / "last-session-stats.json").exists()
    # The isolated store is left intact for anything that still reads it.
    assert (store / "daemon-state" / "embedded-v1" / "index.bin").exists()
    assert "published" in result.stdout


@needs_bash
@pytest.mark.parametrize("case", ["no-build-cache", "no-isolated-store"])
def test_publish_is_a_noop_without_both_ends(tmp_path: Path, case: str) -> None:
    isolated = tmp_path / "isolated"
    build_cache = tmp_path / "build" / "cache" / "zccache"
    if case == "no-isolated-store":
        _write(build_cache / "keep", "restored")
        result = _run_publish(isolated, str(build_cache))
        assert (build_cache / "keep").read_text() == "restored"
    else:
        _write(isolated / "cache" / "zccache" / "a", "x")
        result = _run_publish(isolated, "")
    assert result.returncode == 0, result.stderr
    assert "nothing to publish" in result.stdout
