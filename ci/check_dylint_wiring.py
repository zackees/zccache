"""Reject Dylint libraries omitted from formatting or CI test wiring."""

from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parent.parent


def manifests(root: Path) -> set[str]:
    return {
        path.relative_to(root).as_posix()
        for path in (root / "dylints").glob("*/Cargo.toml")
    }


def check(root: Path = ROOT) -> list[str]:
    """Return wiring errors; kept pure so pytest can exercise fixtures."""
    expected = manifests(root)
    lint = (root / "ci" / "lint.py").read_text(encoding="utf-8")
    workflow = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    errors: list[str] = []
    if 'glob("*/Cargo.toml")' not in lint:
        errors.append("ci/lint.py must discover every dylints/*/Cargo.toml")
    if "ci/check_dylint_wiring.py" not in workflow:
        errors.append("CI dylint job must run ci/check_dylint_wiring.py")
    # The CI dylint job wires each library via an explicit named step (rather
    # than a `dylints/*/Cargo.toml` glob loop) so failures point at the
    # specific library. Every discovered manifest must still appear verbatim
    # in the workflow so no library is silently dropped from formatting/test
    # coverage.
    for manifest in sorted(expected):
        if manifest not in workflow:
            errors.append(f"CI dylint job must format and test {manifest}")
    workspace_file = root / ("Cargo" + ".toml")
    workspace = tomllib.loads(workspace_file.read_text(encoding="utf-8"))
    excluded = set(workspace.get("workspace", {}).get("exclude", []))
    for manifest in expected:
        lint_dir = Path(manifest).parent
        if lint_dir.as_posix() not in excluded:
            errors.append(f"workspace.exclude is missing standalone Dylint: {lint_dir.as_posix()}")
        for required in ("README.md", "rust-toolchain.toml", "src/README.md", "src/lib.rs"):
            if not (root / lint_dir / required).is_file():
                errors.append(f"{lint_dir.as_posix()} is missing {required}")
        source = (root / lint_dir / "src" / "lib.rs").read_text(encoding="utf-8")
        declarations = re.findall(
            r"const\s+\w*SOURCE_PREFIX\w*[^;]+;",
            source,
            flags=re.DOTALL,
        )
        for declaration in declarations:
            for prefix in re.findall(r'"(crates/[^"]+)"', declaration):
                if not (root / prefix).exists():
                    errors.append(
                        f"stale Dylint source prefix: {lint_dir.as_posix()}: {prefix}"
                    )
    for allowlist in (root / "dylints").glob("*/src/allowlist.txt"):
        for line in allowlist.read_text(encoding="utf-8").splitlines():
            entry = line.strip()
            if not entry or entry.startswith("#"):
                continue
            if entry.startswith("crates/") and not (root / entry).exists():
                errors.append(f"stale allowlist path: {allowlist.relative_to(root)}: {entry}")
    return errors


def main() -> int:
    errors = check()
    if errors:
        print("Dylint wiring errors:", file=sys.stderr)
        print("\n".join(f"- {error}" for error in errors), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
