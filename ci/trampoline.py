"""Rust toolchain trampolines.

Routes cargo/rustc/rustfmt/clippy-driver through soldr so the
rustup-managed toolchain is always used, without this module having to
resolve tool paths manually. `ci.env.activate()` is still called first so
repo-local `.rustup` / `.cargo` layouts (vendored toolchains) win over
the user-global location — soldr's `rustup which` honors those env vars.

Why soldr:
- soldr resolves each tool via `rustup which`, matching the existing
  behavior this module used to implement by hand.
- `soldr --no-cache cargo` preserves prior bare-cargo semantics (no
  RUSTC_WRAPPER, no managed zccache inserted). Adopting soldr's built-in
  zccache wrapper here would wrap zccache's own build in a
  previous-version zccache — technically fine, but a deliberate
  decision, not a side effect.
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path

from ci.env import activate


def _soldr_prefix(no_cache: bool):
    """Return the argv prefix that runs soldr, with `--no-cache` if asked."""
    if not shutil.which("soldr"):
        print(
            "error: `soldr` not found on PATH. Run `uv sync` (or ./install) "
            "to install the dev dependencies, which pulls soldr in.",
            file=sys.stderr,
        )
        sys.exit(1)
    prefix = ["soldr"]
    if no_cache:
        prefix.append("--no-cache")
    return prefix


def _run_via_soldr(subcommand: str, *, no_cache: bool):
    """Exec `soldr [--no-cache] <subcommand> <argv...>`."""
    activate()
    cmd = _soldr_prefix(no_cache) + [subcommand] + sys.argv[1:]
    result = subprocess.run(cmd, env=os.environ.copy())
    sys.exit(result.returncode)


def cargo():
    # --no-cache keeps soldr's RUSTC_WRAPPER / zccache path off, matching
    # the previous bare-cargo behavior of this trampoline.
    _run_via_soldr("cargo", no_cache=True)


def rustc():
    _run_via_soldr("rustc", no_cache=False)


def rustfmt():
    _run_via_soldr("rustfmt", no_cache=False)


def clippy_driver():
    _run_via_soldr("clippy-driver", no_cache=False)


def _run_cargo_bin(package):
    """Run a cargo binary with the correct toolchain via soldr."""
    activate()
    extra = sys.argv[1:]
    # Strip leading '--' that uv inserts.
    if extra and extra[0] == "--":
        extra = extra[1:]
    cmd = _soldr_prefix(no_cache=True) + ["cargo", "run", "-p", package]
    if extra:
        cmd.append("--")
        cmd.extend(extra)
    result = subprocess.run(cmd, env=os.environ.copy())
    sys.exit(result.returncode)


def run_zccache():
    _run_cargo_bin("zccache-cli")


def run_zccache_daemon():
    _run_cargo_bin("zccache-daemon")


def run_zccache_fingerprint():
    _run_cargo_bin("zccache-fp")


def check_on_stop():
    _run_cargo_bin("zccache-ci")


def test():
    """Run workspace tests via ci/test.py."""
    script = Path(__file__).parent / "test.py"
    result = subprocess.run([sys.executable, str(script)] + sys.argv[1:])
    sys.exit(result.returncode)


def perf():
    """Run performance benchmarks via ci/perf.py."""
    script = Path(__file__).parent / "perf.py"
    result = subprocess.run([sys.executable, str(script)] + sys.argv[1:])
    sys.exit(result.returncode)
