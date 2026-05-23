"""Shared release metadata validation for lint, test, and publish."""

from __future__ import annotations

import json
import re
import subprocess
import tomllib
from pathlib import Path
from typing import Any

ROOT = Path(__file__).parent.parent.resolve()
RUST_PUBLISH_ORDER = [
    "zccache-monocrate",
    "zccache-download",
    "zccache-download-protocol",
    "zccache-download-daemon",
    "zccache-download-client",
    "zccache-download-cli",
    "zccache-depgraph",
    "zccache-watcher",
    "zccache-fingerprint",
    "zccache-symbols",
    "zccache-cli",
    "zccache-daemon",
]
DYNAMIC_VERSION_PYPROJECTS = (
    ROOT / "pyproject.toml",
    ROOT / "crates" / "zccache-watcher" / "pyproject.toml",
    ROOT / "crates" / "zccache-fingerprint" / "pyproject.toml",
)
INTERNAL_CRATE_PREFIX = "zccache-"
WORKSPACE_DEPENDENCY_RE = re.compile(
    r"^(?P<name>zccache-[A-Za-z0-9_-]+)\s*=\s*\{\s*(?P<body>[^{}\n]*)\s*\}\s*$",
    re.MULTILINE,
)


class ReleaseCheckError(RuntimeError):
    """Raised when release metadata is inconsistent."""


def _read_toml(path: Path) -> dict[str, Any]:
    with open(path, "rb") as f:
        return tomllib.load(f)


def read_workspace_version() -> str:
    data = _read_toml(ROOT / "Cargo.toml")
    return data["workspace"]["package"]["version"]


def read_workspace_metadata() -> dict[str, Any]:
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(result.stdout)


def _internal_workspace_dependencies(
    workspace_data: dict[str, Any],
) -> dict[str, dict[str, Any]]:
    workspace_deps = workspace_data["workspace"].get("dependencies", {})
    return {
        name: spec
        for name, spec in workspace_deps.items()
        if name.startswith(INTERNAL_CRATE_PREFIX)
        and isinstance(spec, dict)
        and "path" in spec
    }


def stamp_internal_dependency_versions(manifest_path: Path | None = None) -> list[str]:
    """Add exact internal dependency pins for crates.io publishing.

    Cargo cannot inherit `[workspace.package].version` inside dependency specs.
    The checked-in manifest omits those copied literals; release publishing calls
    this helper in its disposable checkout before `cargo package`/`publish`.
    """

    manifest_path = manifest_path or ROOT / "Cargo.toml"
    workspace_data = _read_toml(manifest_path)
    version = workspace_data["workspace"]["package"]["version"]
    expected_dependency_version = f"={version}"
    internal_deps = _internal_workspace_dependencies(workspace_data)
    pending = set(internal_deps)

    def stamp_line(match: re.Match[str]) -> str:
        name = match.group("name")
        if name not in internal_deps:
            return match.group(0)
        pending.discard(name)
        body = match.group("body").strip()
        if re.search(r"\bversion\s*=", body):
            body = re.sub(
                r'\bversion\s*=\s*"[^"]*"',
                f'version = "{expected_dependency_version}"',
                body,
                count=1,
            )
        else:
            body = f'version = "{expected_dependency_version}", {body}'
        return f"{name} = {{ {body} }}"

    text = manifest_path.read_text(encoding="utf-8")
    text = WORKSPACE_DEPENDENCY_RE.sub(stamp_line, text)
    if pending:
        raise ReleaseCheckError(
            "Could not stamp internal dependency version(s): "
            + ", ".join(sorted(pending))
        )

    manifest_path.write_text(text, encoding="utf-8")

    stamped_data = _read_toml(manifest_path)
    errors = []
    for name, spec in _internal_workspace_dependencies(stamped_data).items():
        actual = spec.get("version")
        if actual != expected_dependency_version:
            errors.append(
                f"workspace dependency {name} has version {actual}, "
                f"expected {expected_dependency_version}"
            )

    if errors:
        raise ReleaseCheckError(
            "Stamped dependency version checks failed:\n  - "
            + "\n  - ".join(errors)
        )
    return sorted(internal_deps)


def validate_release_versions() -> None:
    workspace_data = _read_toml(ROOT / "Cargo.toml")
    workspace_version = workspace_data["workspace"]["package"]["version"]
    errors: list[str] = []

    for path in DYNAMIC_VERSION_PYPROJECTS:
        project = _read_toml(path)["project"]
        if "version" in project:
            # Hardcoded version — must match workspace
            if project["version"] != workspace_version:
                rel_path = path.relative_to(ROOT)
                errors.append(
                    f"{rel_path} has version {project['version']}, expected {workspace_version}"
                )
        elif "version" not in project.get("dynamic", []):
            rel_path = path.relative_to(ROOT)
            errors.append(
                f"{rel_path} has no version and does not declare it as dynamic"
            )

    for name, spec in _internal_workspace_dependencies(workspace_data).items():
        if "version" in spec:
            errors.append(
                f"workspace dependency {name} duplicates version {spec['version']}; "
                "omit it and let the release publish flow stamp exact pins"
            )

    if errors:
        raise ReleaseCheckError(
            "Release version checks failed:\n  - " + "\n  - ".join(errors)
        )


def validate_rust_publish_order() -> None:
    metadata = read_workspace_metadata()
    packages = metadata.get("packages", [])
    package_by_name = {pkg["name"]: pkg for pkg in packages}
    publishable = {
        pkg["name"]
        for pkg in packages
        if pkg.get("publish") != []
    }
    configured = set(RUST_PUBLISH_ORDER)

    missing = sorted(publishable - configured)
    extra = sorted(configured - publishable)
    errors: list[str] = []
    if missing:
        errors.append(
            f"RUST_PUBLISH_ORDER is missing publishable crates: {', '.join(missing)}"
        )
    if extra:
        errors.append(
            f"RUST_PUBLISH_ORDER contains non-publishable crates: {', '.join(extra)}"
        )

    if not errors:
        order_index = {crate: i for i, crate in enumerate(RUST_PUBLISH_ORDER)}
        # Build a set of mutual (circular) dependency pairs to allow them
        mutual_deps: set[tuple[str, str]] = set()
        for crate in RUST_PUBLISH_ORDER:
            pkg = package_by_name[crate]
            for dep in pkg.get("dependencies", []):
                dep_name = dep["name"]
                if dep_name not in publishable or not dep.get("path"):
                    continue
                dep_pkg = package_by_name.get(dep_name)
                if dep_pkg and any(
                    d["name"] == crate and d.get("path")
                    for d in dep_pkg.get("dependencies", [])
                ):
                    mutual_deps.add((min(crate, dep_name), max(crate, dep_name)))

        for crate in RUST_PUBLISH_ORDER:
            pkg = package_by_name[crate]
            for dep in pkg.get("dependencies", []):
                dep_name = dep["name"]
                if dep_name not in publishable:
                    continue
                if not dep.get("path"):
                    continue
                if dep.get("kind") not in (None, "build"):
                    continue
                pair = (min(crate, dep_name), max(crate, dep_name))
                if pair in mutual_deps:
                    continue  # circular deps are OK (prior version on crates.io)
                if order_index[dep_name] >= order_index[crate]:
                    errors.append(
                        f"RUST_PUBLISH_ORDER schedules {crate} before its dependency {dep_name}"
                    )

    if errors:
        raise ReleaseCheckError(
            "Rust publish-order checks failed:\n  - " + "\n  - ".join(errors)
        )


def validate_release_metadata() -> None:
    validate_release_versions()
    validate_rust_publish_order()
