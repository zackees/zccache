"""Run workspace linting: rustfmt check + clippy.

Usage:
    ./lint                  # fmt, clippy, Dylint for every OS, docs
    ./lint --fix            # auto-fix formatting + clippy (no Dylint)
    ./lint <file.rs>        # single-file rustfmt + per-crate clippy (no Dylint)
    ./lint --dylint-only    # Dylint for the host and every other OS target
"""

import filecmp
import os
import shutil
import subprocess
import sys
from pathlib import Path
from shutil import which

from ci import check_kernal_api_baseline
from ci.env import clean_env
from ci.release_checks import ReleaseCheckError, validate_release_metadata
from ci.soldr import cargo_command, rust_tool_command, self_build_env

SCRIPT_DIR = Path(__file__).parent.parent.resolve()
DYLINT_TOOLCHAIN = "nightly-2026-05-28"
DYLINT_COMPONENTS = ["llvm-tools-preview", "rust-src", "rustc-dev"]
# Dylint's late lints only see cfg-selected code, so a host-only pass misses
# the other operating systems' modules (#1740). One triple per OS is enough:
# ci/check_dylint_wiring.py rejects first-party cfg gates on anything but
# unix/windows/target_os, so every published triple of an OS selects the
# same source.
DYLINT_OS_TARGETS = {
    "linux": "x86_64-unknown-linux-gnu",
    "windows": "x86_64-pc-windows-msvc",
    "macos": "aarch64-apple-darwin",
}


def dylint_manifests() -> list[str]:
    """Return every standalone Dylint library manifest in stable order."""
    return [
        path.relative_to(SCRIPT_DIR).as_posix()
        for path in sorted((SCRIPT_DIR / "dylints").glob("*/Cargo.toml"))
    ]


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
    """Run cargo-dylint through rustup's shim with the pinned nightly selected.

    Dylint deliberately removes ``RUSTUP_TOOLCHAIN`` before building its
    temporary driver crate. The child ``cargo`` must therefore be rustup's
    shim, which rehydrates the variable from that crate's ``rust-toolchain``
    file. A direct toolchain ``cargo`` binary cannot do that, and the Soldr
    front-door shim would route the nested build back through the host wrapper.
    """
    env = self_build_env()
    env["RUSTUP_TOOLCHAIN"] = DYLINT_TOOLCHAIN
    rustup = which("rustup")
    if rustup is None:
        raise FileNotFoundError("rustup is required for workspace dylint")
    rustup_shim_dir = str(Path(rustup).parent)
    env["PATH"] = os.pathsep.join([rustup_shim_dir, env.get("PATH", "")])
    return env


def dylint_command() -> list[str]:
    """Run the Dylint executable while preserving its Cargo subcommand argv."""
    executable = which("cargo-dylint")
    if executable is None:
        raise FileNotFoundError("cargo-dylint is required for workspace linting")
    # The standalone executable parses the same argv shape as the Cargo plugin.
    # Keep the subcommand: without it, Clap delegates `--all` to Cargo itself.
    return [executable, "dylint", "--all", "--workspace"]


def ensure_dylint_aliases():
    """Create cargo-dylint's expected `name@toolchain` aliases when missing."""
    configured_target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    target_dir = (
        configured_target
        if configured_target.is_absolute()
        else SCRIPT_DIR / configured_target
    )
    libraries_root = target_dir / "dylint" / "libraries"
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
            if alias.exists() and filecmp.cmp(library, alias, shallow=False):
                continue
            shutil.copy2(library, alias)
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
    missing = []
    for component in DYLINT_COMPONENTS:
        installed_name = component.removesuffix("-preview")
        if not any(
            line == installed_name or line.startswith(f"{installed_name}-")
            for line in installed
        ):
            missing.append(component)
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


def skip_dylint_on_windows():
    # Retained as a routing seam for callers and tests; every native host now
    # runs the custom late lint, including Windows.
    return False


def dylint_cross_targets() -> list[str]:
    """Return the OS target triples the native host pass does not select."""
    host = {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")
    return [
        target for os_name, target in DYLINT_OS_TARGETS.items() if os_name != host
    ]


def lint_dylint_only():
    """Run the pinned, published Dylint toolchain for every supported OS.

    The native host pass runs first, then one cross-target check per other
    OS. Cross checks never link target code; only build scripts and proc
    macros link, for the host.
    """
    if which("soldr") is None:
        print("soldr is required for Dylint; install it globally", file=sys.stderr)
        return 1
    env = self_build_env()
    env.pop("RUSTFLAGS", None)  # Preserve each lint crate's dylint-link config.
    env.pop("RUSTUP_TOOLCHAIN", None)  # setup-soldr exports stable for CI.
    env["SOLDR_DYLINT_TOOLCHAIN"] = DYLINT_TOOLCHAIN
    env["SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS"] = "1"
    workspace = ["soldr", "dylint", "--all", "--", "--workspace", "--lib", "--bins"]
    cross_targets = dylint_cross_targets()
    for command in (
        ["soldr", "dylint", "prepare"],
        workspace,
        [
            "soldr", "rustup", "target", "add",
            "--toolchain", DYLINT_TOOLCHAIN,
            *cross_targets,
        ],
        *([*workspace, "--target", target] for target in cross_targets),
    ):
        result = subprocess.run(
            command,
            text=True,
            encoding="utf-8",
            errors="replace",
            cwd=str(SCRIPT_DIR),
            env=env,
        )
        if result.returncode != 0:
            return result.returncode
    return 0


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
    """Full workspace lint: fmt check, clippy, Dylint, and doc check."""
    result = run_cmd(cargo_command("fmt", "--all", "--check"))
    if result.returncode != 0:
        print("Formatting issues found. Run './lint --fix' to auto-fix.", file=sys.stderr)
        return result.returncode

    skip_dylint = skip_dylint_on_windows()
    if not skip_dylint:
        for dylint_manifest in (
            "dylints/ban_std_pathbuf/Cargo.toml",
            "dylints/ban_unrooted_tempdir/Cargo.toml",
            "dylints/ban_tmp_literal/Cargo.toml",
            "dylints/ban_raw_subprocess_in_daemon/Cargo.toml",
            "dylints/ban_legacy_artifact_path/Cargo.toml",
            "dylints/ban_normalized_path_deref_containment/Cargo.toml",
            "dylints/ban_dashmap_guard_across_blocking/Cargo.toml",
            "dylints/ban_discarded_write_result/Cargo.toml",
            "dylints/enforce_platform_boundary/Cargo.toml",
            "dylints/ban_registered_env_read/Cargo.toml",
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

    if not skip_dylint:
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

    # The kernal-api inventory guard (CI's "Check kernal-api migration
    # inventory" step) only parses TOML, so run it here too: a direct
    # running-process dependency (#1518) then fails `./lint` and the Stop
    # hook's `ci.lint --fix` before it ever reaches CI.
    inventory_errors = check_kernal_api_baseline.check()
    if inventory_errors:
        print("kernal-api migration inventory errors:", file=sys.stderr)
        print("\n".join(f"- {error}" for error in inventory_errors), file=sys.stderr)
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
