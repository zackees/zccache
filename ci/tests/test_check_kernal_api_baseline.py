import pytest

from pathlib import Path

from ci import check_kernal_api_baseline


def write_captured_baseline(
    root: Path,
    *,
    evidence_revision: str = "revision",
    inventory_revision: str = "revision",
    evidence_toolchain: str = "toolchain",
    inventory_toolchain: str = "toolchain",
) -> None:
    report = root / "docs/architecture/kernal-api-phase-0-baseline.md"
    report.parent.mkdir(parents=True)
    report.write_text("# baseline\n", encoding="utf-8")
    capture = root / "docs/evidence/kernal-api-migration/phase-0/test"
    capture.mkdir(parents=True)
    for artifact in check_kernal_api_baseline.REQUIRED_BASELINE_ARTIFACTS:
        (capture / artifact).write_text("captured\n", encoding="utf-8")
    compiler_prefix = "r" "ustc "
    (capture / "README.md").write_text(
        "Status: captured\n"
        "Captured at: captured-at\n"
        "Host: captured-host\n"
        f"Revision: {evidence_revision}\n"
        f"Toolchain: {compiler_prefix}{evidence_toolchain}\n",
        encoding="utf-8",
    )
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.write_text(
        f'''[baseline]
status = "captured"
report = "docs/architecture/kernal-api-phase-0-baseline.md"
raw_evidence_root = "docs/evidence/kernal-api-migration/phase-0/<host>/<timestamp>/"
capture = "docs/evidence/kernal-api-migration/phase-0/test"
captured_at = "captured-at"
host = "captured-host"
revision = "{inventory_revision}"
toolchain = "{inventory_toolchain}"
feature_sets = ["workspace default"]
commands = ["test"]
result_files = ["clean-build-timing.html", "incremental-build-timing.html", "duplicates.txt", "tokio-reverse-features.txt", "running-process-reverse-features.txt"]
''',
        encoding="utf-8",
    )
    (root / "crates").mkdir(exist_ok=True)


def test_current_inventory_is_complete() -> None:
    assert check_kernal_api_baseline.check() == []


def test_check_rejects_a_missing_baseline_report(tmp_path: Path) -> None:
    root = tmp_path
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.parent.mkdir(parents=True)
    inventory.write_text(
        """[baseline]
status = "captured"
report = "docs/architecture/missing.md"
raw_evidence_root = "docs/evidence/kernal-api-migration/phase-0/<host>/<timestamp>/"
feature_sets = ["workspace default"]
commands = ["soldr cargo build --workspace --timings"]
result_files = ["clean-build-timing.html", "incremental-build-timing.html", "duplicates.txt", "tokio-reverse-features.txt", "running-process-reverse-features.txt"]
""",
        encoding="utf-8",
    )
    (root / "crates").mkdir(exist_ok=True)

    assert any("baseline report missing" in error for error in check_kernal_api_baseline.check(root))


def test_check_rejects_a_captured_baseline_missing_an_artifact(tmp_path: Path) -> None:
    root = tmp_path
    report = root / "docs/architecture/kernal-api-phase-0-baseline.md"
    report.parent.mkdir(parents=True)
    report.write_text("# baseline\n", encoding="utf-8")
    capture = root / "docs/evidence/kernal-api-migration/phase-0/test"
    capture.mkdir(parents=True)
    (capture / "README.md").write_text(
        "Status: captured\nCaptured at: test\nHost: test\n", encoding="utf-8"
    )
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.write_text(
        """[baseline]
status = "captured"
report = "docs/architecture/kernal-api-phase-0-baseline.md"
raw_evidence_root = "docs/evidence/kernal-api-migration/phase-0/<host>/<timestamp>/"
capture = "docs/evidence/kernal-api-migration/phase-0/test"
captured_at = "test"
host = "test"
revision = "test"
toolchain = "test"
feature_sets = ["workspace default"]
commands = ["test"]
result_files = ["clean-build-timing.html", "incremental-build-timing.html", "duplicates.txt", "tokio-reverse-features.txt", "running-process-reverse-features.txt"]
""",
        encoding="utf-8",
    )
    (root / "crates").mkdir(exist_ok=True)

    assert any(
        "baseline capture artifact missing" in error
        for error in check_kernal_api_baseline.check(root)
    )


def test_historical_platform_records_do_not_affect_current_dependency_inventory(
    tmp_path: Path,
) -> None:
    write_captured_baseline(tmp_path)
    inventory = tmp_path / "docs/architecture/kernal-api-migration.toml"
    inventory.write_text(
        inventory.read_text(encoding="utf-8")
        + '''\n[[platform_group]]
source = "crates/zccache-platform/src/platform/host.rs"
items = ["host_fact"]
disposition = "extend"
kernel_capability = "historical evidence only"
''',
        encoding="utf-8",
    )

    assert check_kernal_api_baseline.check(tmp_path) == []


def test_check_rejects_duplicate_backend_dependency_mappings(tmp_path: Path) -> None:
    root = tmp_path
    manifest = root / "crates/example/Cargo.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text('[dependencies]\ntokio = "1"\n', encoding="utf-8")
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.parent.mkdir(parents=True)
    inventory.write_text(
        '''[[backend_dependency]]
name = "tokio"
disposition = "extend"
kernel_capability = "runtime"
manifests = ["crates/example/Cargo.toml"]

[[backend_dependency]]
name = "tokio"
disposition = "extend"
kernel_capability = "runtime"
manifests = ["crates/example/Cargo.toml"]
''',
        encoding="utf-8",
    )

    assert any(
        "duplicate backend dependency mapping:" in error
        for error in check_kernal_api_baseline.check(root)
    )


def test_check_rejects_conflicting_backend_dependency_mappings(tmp_path: Path) -> None:
    root = tmp_path
    manifest = root / "crates/example/Cargo.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text('[dependencies]\ntokio = "1"\n', encoding="utf-8")
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.parent.mkdir(parents=True)
    inventory.write_text(
        '''[[backend_dependency]]
name = "tokio"
disposition = "extend"
kernel_capability = "runtime"
manifests = ["crates/example/Cargo.toml"]

[[backend_dependency]]
name = "tokio"
disposition = "reuse"
kernel_capability = "runtime"
manifests = ["crates/example/Cargo.toml"]
''',
        encoding="utf-8",
    )

    assert any(
        "duplicate backend dependency mapping has conflicting dispositions" in error
        for error in check_kernal_api_baseline.check(root)
    )


def test_check_rejects_evidence_revision_mismatch(tmp_path: Path) -> None:
    write_captured_baseline(
        tmp_path, evidence_revision="captured", inventory_revision="other"
    )

    assert any(
        "baseline capture provenance mismatch" in error and error.endswith(": revision")
        for error in check_kernal_api_baseline.check(tmp_path)
    )


def test_check_rejects_evidence_toolchain_mismatch(tmp_path: Path) -> None:
    write_captured_baseline(
        tmp_path, evidence_toolchain="captured", inventory_toolchain="other"
    )

    assert any(
        "baseline capture provenance mismatch" in error and error.endswith(": toolchain")
        for error in check_kernal_api_baseline.check(tmp_path)
    )


def test_check_accepts_the_documented_compiler_prefix(tmp_path: Path) -> None:
    write_captured_baseline(tmp_path)

    assert not any(
        "baseline capture provenance mismatch" in error
        for error in check_kernal_api_baseline.check(tmp_path)
    )


def write_workspace(root: Path, member_manifest: str, *, workspace_extra: str = "") -> None:
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = ["crates/*"]\n{workspace_extra}',
        encoding="utf-8",
    )
    member = root / "crates" / "member"
    member.mkdir(parents=True)
    (member / "Cargo.toml").write_text(
        f'[package]\nname = "member"\nversion = "0.1.0"\n{member_manifest}',
        encoding="utf-8",
    )


@pytest.mark.parametrize(
    ("member_manifest", "workspace_extra"),
    [
        ('[dependencies]\nrunning-process = "4"\n', ""),
        ('[dependencies]\nrp = { package = "running-process", version = "4" }\n', ""),
        ('[dependencies]\nrunning_process = "4"\n', ""),
        ("[target.'cfg(unix)'.dependencies]\nrunning-process = \"4\"\n", ""),
        ('[dev-dependencies]\nrunning-process = "4"\n', ""),
        ('[build-dependencies]\nrunning-process = "4"\n', ""),
        (
            "[dependencies]\nrunning-process = { workspace = true }\n",
            '[workspace.dependencies]\nrunning-process = "4"\n',
        ),
    ],
    ids=["normal", "renamed", "underscore", "target", "dev", "build", "workspace"],
)
def test_forbidden_backend_is_rejected_on_every_dependency_route(
    tmp_path: Path, member_manifest: str, workspace_extra: str
) -> None:
    write_workspace(tmp_path, member_manifest, workspace_extra=workspace_extra)

    errors = check_kernal_api_baseline.forbidden_direct_dependencies(tmp_path)

    assert errors, "a direct running-process dependency must be rejected"
    assert all("running-process" in error and "#1518" in error for error in errors)


def test_forbidden_backend_is_rejected_from_a_stale_lockfile(tmp_path: Path) -> None:
    write_workspace(tmp_path, "")
    (tmp_path / "Cargo.lock").write_text(
        """version = 4

[[package]]
name = "member"
version = "0.1.0"
dependencies = [
 "running-process 4.10.14",
]
""",
        encoding="utf-8",
    )

    errors = check_kernal_api_baseline.forbidden_direct_dependencies(tmp_path)

    assert errors == [
        "forbidden direct dependency: Cargo.lock: member -> running-process "
        "(reach it only through kernal-api, #1518)"
    ]


def test_forbidden_backend_is_allowed_transitively_through_kernal_api(
    tmp_path: Path,
) -> None:
    write_workspace(tmp_path, '[dependencies]\nkernal-api = "0.1"\n')
    (tmp_path / "Cargo.lock").write_text(
        """version = 4

[[package]]
name = "kernal-api"
version = "0.1.22"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "running-process",
]

[[package]]
name = "member"
version = "0.1.0"
dependencies = [
 "kernal-api",
]
""",
        encoding="utf-8",
    )

    assert check_kernal_api_baseline.forbidden_direct_dependencies(tmp_path) == []


def test_current_workspace_has_no_forbidden_direct_dependency() -> None:
    root = Path(check_kernal_api_baseline.ROOT)
    assert check_kernal_api_baseline.forbidden_direct_dependencies(root) == []
