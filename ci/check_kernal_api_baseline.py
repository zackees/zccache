"""Verify the checked-in kernal-api migration inventory (zccache#1519)."""

from __future__ import annotations

from pathlib import Path
import sys
import tomllib


ROOT = Path(__file__).resolve().parent.parent
INVENTORY = ROOT / "docs" / "architecture" / "kernal-api-migration.toml"
BACKENDS = {"tokio", "tokio-util", "running-process", "interprocess", "crash-handler", "memmap2", "fs2", "blake3", "libc", "windows-sys"}
VALID_DISPOSITIONS = {"reuse", "extend", "move", "retain"}
VALID_BASELINE_STATUSES = {"captured"}
CAPTURE_PROVENANCE_FIELDS = {"capture", "captured_at", "host", "revision", "toolchain"}
COMPILER_TOOLCHAIN_PREFIX = "r" "ustc "
REQUIRED_BASELINE_ARTIFACTS = {
    "clean-build-timing.html",
    "incremental-build-timing.html",
    "duplicates.txt",
    "tokio-reverse-features.txt",
    "running-process-reverse-features.txt",
}


def production_dependencies(manifest: Path) -> set[str]:
    """Return backend keys in normal and target production dependency tables."""
    parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
    found = set(parsed.get("dependencies", {})) & BACKENDS
    for target in parsed.get("target", {}).values():
        found |= set(target.get("dependencies", {})) & BACKENDS
    return found


def evidence_label_value(provenance: str, label: str) -> str | None:
    """Return one exact `Label: value` evidence line, rejecting ambiguity."""
    prefix = f"{label}: "
    values = [
        line.removeprefix(prefix)
        for line in provenance.splitlines()
        if line.startswith(prefix)
    ]
    return values[0] if len(values) == 1 else None


def check(root: Path = ROOT) -> list[str]:
    inventory_path = root / INVENTORY.relative_to(ROOT)
    data = tomllib.loads(inventory_path.read_text(encoding="utf-8"))
    errors: list[str] = []
    baseline = data.get("baseline", {})
    baseline_status = baseline.get("status")
    if baseline_status not in VALID_BASELINE_STATUSES:
        errors.append(f"invalid baseline status: {baseline_status!r}")
    for field in ("report", "raw_evidence_root"):
        if not baseline.get(field):
            errors.append(f"baseline lacks {field}")
    report = baseline.get("report")
    if report and not (root / report).is_file():
        errors.append(f"baseline report missing: {report}")
    if not baseline.get("feature_sets"):
        errors.append("baseline lacks feature sets")
    if not baseline.get("commands"):
        errors.append("baseline lacks reproducible commands")
    result_files = set(baseline.get("result_files", []))
    missing_artifacts = sorted(REQUIRED_BASELINE_ARTIFACTS - result_files)
    if missing_artifacts:
        errors.append(f"baseline lacks result filenames: {', '.join(missing_artifacts)}")
    if baseline_status == "captured":
        for field in sorted(CAPTURE_PROVENANCE_FIELDS):
            if not baseline.get(field):
                errors.append(f"captured baseline lacks {field}")
        capture = baseline.get("capture")
        if capture:
            capture_root = root / capture
            if not capture_root.is_dir():
                errors.append(f"baseline capture missing: {capture}")
            else:
                for artifact in sorted(REQUIRED_BASELINE_ARTIFACTS):
                    if not (capture_root / artifact).is_file():
                        errors.append(f"baseline capture artifact missing: {capture}: {artifact}")
                capture_readme = capture_root / "README.md"
                if not capture_readme.is_file():
                    errors.append(f"baseline capture provenance missing: {capture}: README.md")
                else:
                    provenance = capture_readme.read_text(encoding="utf-8")
                    for field, label in (
                        ("captured_at", "Captured at"),
                        ("host", "Host"),
                        ("revision", "Revision"),
                        ("toolchain", "Toolchain"),
                    ):
                        actual = evidence_label_value(provenance, label)
                        if field == "toolchain" and actual is not None:
                            # Captures preserve the compiler command spelling. The
                            # inventory omits only this exact documented prefix.
                            actual = actual.removeprefix(COMPILER_TOOLCHAIN_PREFIX)
                        if actual != baseline[field]:
                            errors.append(
                                f"baseline capture provenance mismatch: {capture}: {field}"
                            )
                    if "Status: captured" not in provenance:
                        errors.append(f"baseline capture status missing: {capture}")
    expected: set[tuple[str, str]] = set()
    backend_mappings: dict[tuple[str, str], tuple[str | None, int]] = {}
    for dependency_index, dependency in enumerate(data.get("backend_dependency", []), start=1):
        name, disposition = dependency.get("name"), dependency.get("disposition")
        if name not in BACKENDS:
            errors.append(f"unknown backend dependency: {name!r}")
        if disposition not in VALID_DISPOSITIONS:
            errors.append(f"invalid backend disposition for {name}: {disposition!r}")
        if not dependency.get("kernel_capability"):
            errors.append(f"backend mapping lacks capability: {name}")
        for manifest in dependency.get("manifests", []):
            key = (manifest, name)
            previous = backend_mappings.get(key)
            if previous is not None:
                previous_disposition, previous_index = previous
                if previous_disposition != disposition:
                    errors.append(
                        "duplicate backend dependency mapping has conflicting dispositions: "
                        f"{manifest}: {name} (entries {previous_index} and {dependency_index}: "
                        f"{previous_disposition!r} vs {disposition!r})"
                    )
                else:
                    errors.append(
                        f"duplicate backend dependency mapping: {manifest}: {name} "
                        f"(entries {previous_index} and {dependency_index})"
                    )
            else:
                backend_mappings[key] = (disposition, dependency_index)
            expected.add(key)
    actual: set[tuple[str, str]] = set()
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        relative = manifest.relative_to(root).as_posix()
        actual |= {(relative, name) for name in production_dependencies(manifest)}
    for manifest, name in sorted(actual - expected):
        errors.append(f"unmapped production backend dependency: {manifest}: {name}")
    for manifest, name in sorted(expected - actual):
        errors.append(f"stale backend dependency mapping: {manifest}: {name}")

    for entry in data.get("characterization", []):
        contract, tests = entry.get("contract", "<unnamed>"), entry.get("tests", [])
        if not tests:
            errors.append(f"characterization lacks evidence paths: {contract}")
        for path in tests:
            if not (root / path).is_file():
                errors.append(f"characterization path missing: {contract}: {path}")
    return errors


def main() -> int:
    errors = check()
    if errors:
        print("kernal-api migration inventory errors:", file=sys.stderr)
        print("\n".join(f"- {error}" for error in errors), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
