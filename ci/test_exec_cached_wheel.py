"""Installed-wheel smoke contract for ``zccache.exec_cached`` (#1433).

The release workflow runs this file on Linux, macOS, and Windows after
installing the wheel assembled from that platform's native artifacts.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import threading
import uuid
from pathlib import Path


def _run(binary: str, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [binary, *args],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )


def main() -> None:
    import zccache

    binary = shutil.which("zccache")
    if binary is None:
        raise AssertionError("installed wheel did not place zccache on PATH")

    namespace = f"python-exec-cached-{uuid.uuid4().hex}"
    with tempfile.TemporaryDirectory(prefix="zccache-exec-cached-") as temp:
        os.environ["ZCCACHE_CACHE_DIR"] = str(Path(temp) / "cache")
        os.environ["ZCCACHE_DAEMON_NAMESPACE"] = namespace
        source = Path(temp) / "input.txt"
        source.write_bytes(b"input-v1")

        _run(binary, "start")
        try:
            calls = 0

            def runner() -> bytes:
                nonlocal calls
                calls += 1
                return b"opaque-result"

            kwargs = {
                "name": "wheel-smoke",
                "input_files": [source],
                "input_env": {"MODE": "strict"},
                "extra_key": b"schema-v1",
                "runner": runner,
            }
            assert zccache.exec_cached(**kwargs) == b"opaque-result"
            assert zccache.exec_cached(**kwargs) == b"opaque-result"
            assert calls == 1, "warm hit must not invoke the Python runner"

            _run(binary, "stop")
            _run(binary, "start")
            assert zccache.exec_cached(**kwargs) == b"opaque-result"
            assert calls == 1, "stored bytes must survive a daemon restart"

            barrier = threading.Barrier(2)
            parallel_results: list[bytes] = []
            parallel_errors: list[BaseException] = []

            def parallel_call(index: int) -> None:
                def parallel_runner() -> bytes:
                    barrier.wait(timeout=10)
                    return f"parallel-{index}".encode()

                try:
                    result = zccache.exec_cached(
                        "wheel-parallel",
                        [source],
                        {"MODE": "strict"},
                        f"key-{index}".encode(),
                        parallel_runner,
                    )
                    parallel_results.append(result)
                except BaseException as error:  # retain thread failures for the main thread
                    parallel_errors.append(error)

            threads = [threading.Thread(target=parallel_call, args=(index,)) for index in range(2)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=15)
            assert not parallel_errors, parallel_errors
            assert all(not thread.is_alive() for thread in threads), "parallel calls deadlocked"
            assert sorted(parallel_results) == [b"parallel-0", b"parallel-1"]

            class RunnerFailure(Exception):
                pass

            def failing_runner() -> bytes:
                raise RunnerFailure("runner failed exactly")

            try:
                zccache.exec_cached(
                    "wheel-exception",
                    [source],
                    {},
                    b"exception",
                    failing_runner,
                )
            except RunnerFailure as error:
                assert str(error) == "runner failed exactly"
            else:
                raise AssertionError("runner exception was not propagated")

            missing_namespace = f"{namespace}-missing"
            os.environ["ZCCACHE_DAEMON_NAMESPACE"] = missing_namespace
            unavailable_runner_called = False

            def unavailable_runner() -> bytes:
                nonlocal unavailable_runner_called
                unavailable_runner_called = True
                return b"must-not-run"

            try:
                zccache.exec_cached(
                    "wheel-unavailable",
                    [source],
                    {},
                    b"missing-daemon",
                    unavailable_runner,
                )
            except RuntimeError as error:
                assert "daemon" in str(error).lower() or "connect" in str(error).lower()
            else:
                raise AssertionError("daemon-unavailable call unexpectedly succeeded")
            assert not unavailable_runner_called
        finally:
            os.environ["ZCCACHE_DAEMON_NAMESPACE"] = namespace
            subprocess.run(
                [binary, "stop"],
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )


if __name__ == "__main__":
    main()
