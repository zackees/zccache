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


PLATFORM_BASELINE_KINDS = frozenset(
    {"attr_cfg", "cfg_macro", "native_import", "module_ref"}
)


def check_platform_baseline(root: Path, path: Path) -> list[str]:
    """Validate enforce_platform_boundary's exact-occurrence baseline.

    Each row is `path<TAB>kind<TAB>normalized<TAB>ordinal`. Rows must point at
    existing production sources (never the platform crate's allowed zones or
    test trees), must be unique, and ordinals must be contiguous from zero
    per (path, kind, normalized) group. The header's total must match.
    """
    errors: list[str] = []
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    declared_total: int | None = None
    seen: set[tuple[str, str, str, str]] = set()
    ordinals: dict[tuple[str, str, str], list[int]] = {}
    entry_count = 0
    for lineno, line in enumerate(lines, 1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            if stripped.startswith("# total ="):
                try:
                    declared_total = int(stripped.removeprefix("# total ="))
                except ValueError:
                    errors.append(f"platform baseline total is not an integer: {lineno}")
            continue
        fields = line.split("\t")
        if len(fields) != 4:
            errors.append(f"platform baseline row must have 4 fields: {lineno}")
            continue
        file_path, kind, normalized, ordinal_text = fields
        entry_count += 1
        if kind not in PLATFORM_BASELINE_KINDS:
            errors.append(f"platform baseline unknown kind {kind!r}: {lineno}")
        if not file_path.startswith("crates/"):
            errors.append(f"platform baseline path outside crates/: {file_path}")
            continue
        target = root / file_path
        if not target.is_file():
            errors.append(f"stale platform-boundary baseline path: {file_path}")
        if file_path.startswith("crates/zccache-platform/src/"):
            errors.append(
                f"platform baseline entry in an allowed zone: {file_path}"
            )
        if "/tests/" in file_path or file_path.endswith("_tests.rs") or "/benches/" in file_path:
            errors.append(f"platform baseline entry outside production scope: {file_path}")
        row = (file_path, kind, normalized, ordinal_text)
        if row in seen:
            errors.append(f"platform baseline duplicate row: {line.strip()}")
        seen.add(row)
        try:
            ordinal = int(ordinal_text)
        except ValueError:
            errors.append(f"platform baseline ordinal is not an integer: {line.strip()}")
            continue
        ordinals.setdefault((file_path, kind, normalized), []).append(ordinal)
    for key, values in ordinals.items():
        expected = list(range(len(values)))
        if sorted(values) != expected:
            errors.append(
                f"platform baseline ordinals are not contiguous from zero "
                f"for {key}: {sorted(values)}"
            )
    if declared_total is not None and declared_total != entry_count:
        errors.append(
            f"platform baseline total {declared_total} != {entry_count} rows"
        )
    return errors


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
    for baseline in (root / "dylints").glob("*/src/baseline.txt"):
        errors.extend(check_platform_baseline(root, baseline))
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
