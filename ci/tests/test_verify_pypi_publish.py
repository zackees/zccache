"""Tests for `ci.release_workflow.verify_pypi_publish` (issue #483).

The function polls PyPI's release JSON until every expected wheel is
visible, closing the upload-race window where downstream `uv sync` would
otherwise cache a partial wheel set.

These tests inject fake `fetch` / `clock` / `sleep` callables so:
- No network is touched.
- The clock advances deterministically without real sleeping.
- The polling/timeout/HTTP-404-tolerance branches each get a dedicated
  scenario.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import urllib.error
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]


def _load_release_workflow():
    # Import via a fresh module spec so a parent `from ci.release_workflow
    # import ...` elsewhere can't shadow the version under test.
    spec = importlib.util.spec_from_file_location(
        "ci.release_workflow",
        REPO_ROOT / "ci" / "release_workflow.py",
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["ci.release_workflow"] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture(scope="module")
def rw():
    return _load_release_workflow()


def _json_body(filenames: list[str]) -> bytes:
    return json.dumps({"urls": [{"filename": f} for f in filenames]}).encode()


# ── fetch_pypi_release_filenames ──────────────────────────────────────────


def test_fetch_returns_filenames_from_json(rw) -> None:
    expected = ["zccache-1.0-cp310-cp310-manylinux_2_17_x86_64.whl",
                "zccache-1.0-cp310-cp310-win_amd64.whl"]
    captured: list[str] = []

    def fake_fetch(url: str) -> bytes:
        captured.append(url)
        return _json_body(expected)

    result = rw.fetch_pypi_release_filenames("zccache", "1.0", fetch=fake_fetch)

    assert result == set(expected)
    assert captured == ["https://pypi.org/pypi/zccache/1.0/json"]


def test_fetch_treats_404_as_empty_release(rw) -> None:
    def fake_fetch(_url: str) -> bytes:
        raise urllib.error.HTTPError(
            url=_url, code=404, msg="Not Found", hdrs=None, fp=None  # type: ignore[arg-type]
        )

    result = rw.fetch_pypi_release_filenames("zccache", "9.9.9", fetch=fake_fetch)
    assert result == set(), "404 must surface as empty so callers keep polling"


def test_fetch_propagates_non_404_http_errors(rw) -> None:
    def fake_fetch(_url: str) -> bytes:
        raise urllib.error.HTTPError(
            url=_url, code=503, msg="Service Unavailable", hdrs=None, fp=None  # type: ignore[arg-type]
        )

    with pytest.raises(urllib.error.HTTPError):
        rw.fetch_pypi_release_filenames("zccache", "1.0", fetch=fake_fetch)


# ── verify_pypi_publish ────────────────────────────────────────────────────


def test_verify_returns_immediately_when_all_wheels_visible(rw) -> None:
    expected = ["a.whl", "b.whl", "c.whl"]
    fetches: list[str] = []

    def fake_fetch(url: str) -> bytes:
        fetches.append(url)
        return _json_body(expected)

    sleeps: list[float] = []
    rw.verify_pypi_publish(
        "zccache",
        "1.0",
        expected,
        timeout_s=60.0,
        poll_interval_s=5.0,
        fetch=fake_fetch,
        clock=lambda: 0.0,
        sleep=lambda s: sleeps.append(s),
    )

    assert len(fetches) == 1, "should not poll twice when everything visible on first try"
    assert sleeps == [], "should not sleep when first poll succeeds"


def test_verify_polls_until_all_wheels_appear(rw) -> None:
    expected = ["a.whl", "b.whl", "c.whl"]
    # Sequential uploads visible to us: first poll sees 1/3, second 2/3,
    # third 3/3. Mirrors the actual `pypa/gh-action-pypi-publish` upload
    # pattern that this fix closes the race for.
    visibility = [["a.whl"], ["a.whl", "b.whl"], expected]
    poll_idx = [0]

    def fake_fetch(_url: str) -> bytes:
        body = _json_body(visibility[min(poll_idx[0], len(visibility) - 1)])
        poll_idx[0] += 1
        return body

    sleeps: list[float] = []
    now = [0.0]

    rw.verify_pypi_publish(
        "zccache",
        "1.0",
        expected,
        timeout_s=600.0,
        poll_interval_s=5.0,
        fetch=fake_fetch,
        clock=lambda: now[0],
        sleep=lambda s: (sleeps.append(s), now.__setitem__(0, now[0] + s))[1],
    )

    # Three poll attempts (1/3 → 2/3 → 3/3); two sleeps between them.
    assert poll_idx[0] == 3
    assert sleeps == [5.0, 5.0]


def test_verify_raises_runtime_error_on_timeout(rw) -> None:
    expected = ["a.whl", "b.whl"]

    def fake_fetch(_url: str) -> bytes:
        # Always partial — only one wheel ever appears.
        return _json_body(["a.whl"])

    now = [0.0]

    def fake_clock() -> float:
        return now[0]

    def fake_sleep(s: float) -> None:
        now[0] += s

    with pytest.raises(RuntimeError, match=r"timed out after 30s"):
        rw.verify_pypi_publish(
            "zccache",
            "1.0",
            expected,
            timeout_s=30.0,
            poll_interval_s=10.0,
            fetch=fake_fetch,
            clock=fake_clock,
            sleep=fake_sleep,
        )


def test_verify_timeout_message_lists_missing_wheels(rw) -> None:
    expected = ["a.whl", "b.whl", "c.whl"]

    def fake_fetch(_url: str) -> bytes:
        return _json_body(["a.whl"])

    now = [0.0]

    def fake_clock() -> float:
        return now[0]

    def fake_sleep(s: float) -> None:
        now[0] += s

    with pytest.raises(RuntimeError) as excinfo:
        rw.verify_pypi_publish(
            "zccache",
            "1.0",
            expected,
            timeout_s=10.0,
            poll_interval_s=20.0,
            fetch=fake_fetch,
            clock=fake_clock,
            sleep=fake_sleep,
        )

    msg = str(excinfo.value)
    assert "b.whl" in msg
    assert "c.whl" in msg
    assert "a.whl" not in msg, "the seen wheel should not appear in the missing list"


def test_verify_rejects_empty_expected_list(rw) -> None:
    # Empty list = caller bug (e.g. wheels-dir was empty). Surface it
    # before we hit PyPI; the workflow should fail loudly, not poll
    # forever / accept whatever happens to be there.
    with pytest.raises(SystemExit, match="no expected wheels"):
        rw.verify_pypi_publish("zccache", "1.0", [], fetch=lambda _u: _json_body([]))


def test_verify_tolerates_initial_404(rw) -> None:
    # Realistic timing: PyPI's release JSON is 404 for a brief window
    # after the first wheel uploads (cache propagation). The fix must
    # poll through that, not bail.
    expected = ["a.whl"]
    state = {"calls": 0}

    def fake_fetch(_url: str) -> bytes:
        state["calls"] += 1
        if state["calls"] == 1:
            raise urllib.error.HTTPError(
                url=_url, code=404, msg="Not Found", hdrs=None, fp=None  # type: ignore[arg-type]
            )
        return _json_body(expected)

    now = [0.0]
    rw.verify_pypi_publish(
        "zccache",
        "1.0",
        expected,
        timeout_s=60.0,
        poll_interval_s=1.0,
        fetch=fake_fetch,
        clock=lambda: now[0],
        sleep=lambda s: now.__setitem__(0, now[0] + s),
    )

    assert state["calls"] == 2
