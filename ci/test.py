"""Run workspace tests.

Usage:
    ./test                          # unit tests only (fast, no compiler needed)
    ./test --integration            # integration tests only (need clang)
    ./test --full                   # unit + integration + stress tests
    ./test -p zccache-hash          # single crate
    ./test -p zccache-hash -- name  # single test by name
"""

import subprocess
import sys
from pathlib import Path

from ci.release_checks import ReleaseCheckError, validate_release_metadata
from ci.soldr import cargo_command, self_build_env

SCRIPT_DIR = Path(__file__).parent.parent.resolve()

# Feature set that enables every `required-features` [[bin]] in crates/zccache
# (mirrors the list used by ci.yml's lib+bins test step). Tests under
# crates/zccache/tests spawn these binaries via `env!("CARGO_BIN_EXE_*")` —
# cargo bakes the path at compile time but does NOT build required-features
# bins as part of `cargo test`, so on a fresh checkout the spawn fails with
# NotFound (issue #1030: zccache-daemon, zccache, cli-crash-trigger, ...).
PREBUILD_BIN_FEATURES = (
    "zccache-bin,daemon-bin,download-bin,download-daemon-bin,"
    "fingerprint-bin,stamp-bin,ci-bin,crash-tools,tokio-console,test-support"
)


def prebuild_test_helper_bins() -> int:
    cmd = cargo_command("build") + [
        "-p",
        "zccache",
        "--bins",
        "--features",
        PREBUILD_BIN_FEATURES,
    ]
    result = subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=self_build_env(),
    )
    return result.returncode


def main():
    try:
        validate_release_metadata()
    except ReleaseCheckError as e:
        print(str(e), file=sys.stderr)
        return 1

    rc = prebuild_test_helper_bins()
    if rc != 0:
        return rc

    cmd = cargo_command("test")

    args = sys.argv[1:]
    full = "--full" in args
    integration = "--integration" in args
    if full:
        args.remove("--full")
    if integration:
        args.remove("--integration")

    # Split on "--" to separate cargo args from test-binary args.
    if "--" in args:
        sep = args.index("--")
        cargo_args = args[:sep]
        test_args = args[sep + 1:]
    else:
        cargo_args = args
        test_args = []

    if not cargo_args:
        cmd += ["--workspace"]

    cmd += cargo_args
    cmd += ["--"]
    if full:
        cmd += ["--include-ignored"]
    elif integration:
        cmd += ["--ignored"]
        cmd += [
            "--skip",
            "bench_",
            "--skip",
            "perf_",
            "--skip",
            "stress_",
            "--skip",
            "_stress",
        ]
    if not any(
        arg == "--test-threads" or arg.startswith("--test-threads=")
        for arg in test_args
    ):
        cmd += ["--test-threads=1"]
    cmd += test_args

    result = subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=self_build_env(),
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
