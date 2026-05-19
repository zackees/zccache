"""Run workspace linting: rustfmt check + clippy.

Usage:
    ./lint              # full workspace lint
    ./lint --fix        # auto-fix formatting + clippy
    ./lint <file.rs>    # single-file rustfmt + per-crate clippy
"""

import os
import subprocess
import sys
from pathlib import Path
from shutil import which

from ci.env import clean_env
from ci.release_checks import ReleaseCheckError, validate_release_metadata
from ci.soldr import cargo_command, rust_tool_command, self_build_env

SCRIPT_DIR = Path(__file__).parent.parent.resolve()
DYLINT_TOOLCHAIN = "nightly-2026-03-26"
DYLINT_COMPONENTS = ["llvm-tools-preview", "rust-src", "rustc-dev"]


def is_soldr_cargo_command(cmd):
    return (
        len(cmd) >= 2
        and Path(cmd[0]).name.startswith("soldr")
        and cmd[1] == "cargo"
    )


def run_cmd(cmd):
    """Run a command rooted at the project directory."""
    env = self_build_env() if is_soldr_cargo_command(cmd) else clean_env()
    return subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=env,
    )


def run_cmd_capture(cmd):
    """Run a command rooted at the project directory and capture output."""
    env = self_build_env() if is_soldr_cargo_command(cmd) else clean_env()
    return subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=env,
        capture_output=True,
    )


def dylint_env():
    """Run cargo-dylint under the nightly toolchain without using +toolchain syntax."""
    env = self_build_env()
    env["RUSTUP_TOOLCHAIN"] = DYLINT_TOOLCHAIN
    return env


def ensure_dylint_aliases():
    """Create cargo-dylint's expected `name@toolchain` aliases when missing."""
    libraries_root = SCRIPT_DIR / "target" / "dylint" / "libraries"
    if not libraries_root.is_dir():
        return False

    created = False
    for toolchain_dir in libraries_root.iterdir():
        if not toolchain_dir.is_dir():
            continue
        release_dir = toolchain_dir / "release"
        if not release_dir.is_dir():
            continue
        for library in release_dir.iterdir():
            if not library.is_file():
                continue
            if library.suffix not in {".dll", ".dylib", ".so"}:
                continue
            if "@" in library.stem:
                continue
            alias = library.with_name(
                f"{library.stem}@{toolchain_dir.name}{library.suffix}"
            )
            if alias.exists():
                continue
            alias.write_bytes(library.read_bytes())
            created = True
    return created


def ensure_dylint_components():
    """Install the Rust components required to build the workspace dylint."""
    if which("rustup") is None:
        print(
            "rustup is required for workspace dylint setup.",
            file=sys.stderr,
        )
        return 1

    result = run_cmd_capture([
        "rustup", "component", "list",
        "--toolchain", DYLINT_TOOLCHAIN,
        "--installed",
    ])
    if result.returncode != 0:
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        return result.returncode

    installed = result.stdout.splitlines()
    missing = [
        component
        for component in DYLINT_COMPONENTS
        if not any(line.startswith(component) for line in installed)
    ]
    if not missing:
        return 0

    print(
        "Installing missing Rust components for dylint: "
        + ", ".join(missing),
        file=sys.stderr,
    )
    result = run_cmd([
        "rustup", "component", "add",
        "--toolchain", DYLINT_TOOLCHAIN,
        *missing,
    ])
    return result.returncode


def lint_dylint_only():
    """Run workspace dylint, retrying after alias repair if cargo-dylint misses it."""
    if which("cargo-dylint") is None:
        print(
            "cargo-dylint is required for workspace linting. Install with "
            "'cargo install cargo-dylint dylint-link'.",
            file=sys.stderr,
        )
        return 1

    result = ensure_dylint_components()
    if result != 0:
        return result

    dylint_cmd = cargo_command("dylint", "--all", "--workspace")

    # cargo-dylint expects libraries on disk as `<name>@<toolchain>.<ext>` but
    # cargo emits them as bare `<name>.<ext>`. Each freshly-built library
    # therefore fails the first time it runs. After every build cycle we
    # alias the just-built libraries and retry. With N dylints in the
    # workspace the worst case is N+1 invocations — each retry compiles the
    # next library fresh, so we loop until no new aliases need creating.
    max_attempts = 6
    last_result = None
    for attempt in range(1, max_attempts + 1):
        capture = attempt < max_attempts
        result = subprocess.run(
            dylint_cmd,
            text=True,
            encoding="utf-8",
            errors="replace",
            cwd=str(SCRIPT_DIR),
            env=dylint_env(),
            capture_output=capture,
        )
        if capture:
            sys.stdout.write(result.stdout or "")
            sys.stderr.write(result.stderr or "")
        last_result = result
        if result.returncode == 0:
            break
        if not ensure_dylint_aliases():
            # No new aliases to create — the failure isn't a missing-alias
            # one, so further retries won't help.
            break
    return last_result.returncode if last_result is not None else 1


def detect_crate(file_path):
    """Extract crate name from a file path under crates/."""
    normalized = file_path.replace("\\", "/")
    if "crates/" in normalized:
        parts = normalized.split("crates/")
        if len(parts) > 1:
            crate_dir = parts[1].split("/")[0]
            if crate_dir:
                return crate_dir
    return None


def lint_single_file(file_path):
    """Lint a single .rs file: rustfmt + per-crate clippy."""
    file_path = os.path.abspath(file_path)

    if not file_path.endswith(".rs"):
        print(f"Skipping non-Rust file: {file_path}", file=sys.stderr)
        return 0

    if not os.path.isfile(file_path):
        print(f"File not found: {file_path}", file=sys.stderr)
        return 1

    result = run_cmd(rust_tool_command("rustfmt", file_path))
    if result.returncode != 0:
        return result.returncode

    crate = detect_crate(file_path)
    cmd = cargo_command("clippy")
    if crate:
        cmd += ["-p", crate]
    else:
        cmd += ["--workspace"]
    cmd += ["--all-targets", "--", "-D", "warnings"]

    result = run_cmd(cmd)
    return result.returncode


def lint_workspace():
    """Full workspace lint: fmt check + clippy + doc check."""
    result = run_cmd(cargo_command("fmt", "--all", "--check"))
    if result.returncode != 0:
        print("Formatting issues found. Run './lint --fix' to auto-fix.", file=sys.stderr)
        return result.returncode

    for dylint_manifest in (
        "dylints/ban_std_pathbuf/Cargo.toml",
        "dylints/ban_unrooted_tempdir/Cargo.toml",
    ):
        result = run_cmd(cargo_command(
            "fmt",
            "--manifest-path", dylint_manifest,
            "--all", "--check",
        ))
        if result.returncode != 0:
            print(
                f"Dylint library formatting issues found in {dylint_manifest}.",
                file=sys.stderr,
            )
            return result.returncode

    result = run_cmd(cargo_command(
        "clippy", "--workspace", "--all-targets",
        "--", "-D", "warnings",
    ))
    if result.returncode != 0:
        return result.returncode

    if os.name == "nt":
        print(
            "Skipping workspace dylint on Windows; the dedicated Dylint CI job runs on Ubuntu.",
            file=sys.stderr,
        )
    else:
        result = lint_dylint_only()
        if result != 0:
            return result

    env = self_build_env()
    env["RUSTDOCFLAGS"] = "-D warnings"
    result = subprocess.run(
        cargo_command("doc", "--workspace", "--no-deps"),
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=env,
    )
    return result.returncode


def main():
    try:
        validate_release_metadata()
    except ReleaseCheckError as e:
        print(str(e), file=sys.stderr)
        return 1

    args = sys.argv[1:]

    if "--fix" in args:
        args.remove("--fix")
        result = run_cmd(cargo_command("fmt", "--all"))
        if result.returncode != 0:
            return result.returncode
        if not args:
            # Stop-hook case: --fix with no positional args.
            # Run clippy ONCE here and return — do NOT fall through to
            # lint_workspace() below (which also runs clippy + dylint + doc)
            # otherwise clippy would run twice. See #139 fix 5.
            result = run_cmd(cargo_command(
                "clippy", "--workspace", "--all-targets",
                "--", "-D", "warnings",
            ))
            return result.returncode

    if args and args[0].endswith(".rs"):
        return lint_single_file(args[0])

    if args == ["--dylint-only"]:
        return lint_dylint_only()

    return lint_workspace()


if __name__ == "__main__":
    sys.exit(main())
