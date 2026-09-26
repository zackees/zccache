"""#1601: the perf harness aligns soldr's exact pins with the checkout under test."""

from pathlib import Path

from ci import perf_local, perf_local_pins


def test_exact_workspace_pins_reads_only_exact_requirements(tmp_path: Path) -> None:
    (tmp_path / "Cargo.toml").write_text(
        "[workspace.dependencies]\n"
        'kernal-api = { version = "=0.1.22", default-features = false }\n'
        'serde = "1"\n'
        'exact-plain = "=2.0.0"\n'
        'path-only = { path = "crates/x" }\n',
        encoding="utf-8",
    )
    assert perf_local_pins.exact_workspace_pins(tmp_path) == {
        "kernal-api": "0.1.22",
        "exact-plain": "2.0.0",
    }


def test_the_real_checkout_pins_kernal_api_exactly() -> None:
    pins = perf_local_pins.exact_workspace_pins(perf_local.REPO_ROOT)
    assert "kernal-api" in pins


def test_align_rewrites_only_diverging_exact_pins(tmp_path: Path) -> None:
    soldr = tmp_path / "soldr-src"
    platform = soldr / "crates" / "soldr-platform" / "Cargo.toml"
    cli = soldr / "crates" / "soldr-cli" / "Cargo.toml"
    vendored = soldr / "_vender" / "x" / "Cargo.toml"
    for manifest in (platform, cli, vendored):
        manifest.parent.mkdir(parents=True)
    platform.write_text(
        "[dependencies]\n"
        'kernal-api = { version = "=0.1.20", default-features = false, features = ["fs"] }\n'
        'kernal-api-extra = "=0.1.20"\n'
        'serde = "1"\n',
        encoding="utf-8",
    )
    cli.write_text('[dependencies]\nkernal-api = "^0.1.20"\n', encoding="utf-8")
    vendored.write_text('[dependencies]\nkernal-api = "=0.1.20"\n', encoding="utf-8")

    changes = perf_local_pins.align_soldr_exact_pins(soldr, {"kernal-api": "0.1.22"})

    text = platform.read_text(encoding="utf-8")
    assert (
        'kernal-api = { version = "=0.1.22", default-features = false, features = ["fs"] }'
        in text
    )
    assert 'kernal-api-extra = "=0.1.20"' in text
    # A caret requirement is soldr's to own; only exact pins conflict.
    assert cli.read_text(encoding="utf-8") == '[dependencies]\nkernal-api = "^0.1.20"\n'
    assert (
        vendored.read_text(encoding="utf-8")
        == '[dependencies]\nkernal-api = "=0.1.20"\n'
    )
    assert changes == [
        "crates/soldr-platform/Cargo.toml: kernal-api =0.1.20 -> =0.1.22"
    ]


def test_pin_soldr_source_aligns_shared_exact_pins(tmp_path: Path, monkeypatch) -> None:
    soldr = tmp_path / "soldr-src"
    manifest = soldr / "crates" / "soldr-platform" / "Cargo.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text('[dependencies]\nkernal-api = "=0.0.1"\n', encoding="utf-8")
    monkeypatch.setattr(perf_local, "git_is_dirty", lambda _repo: False)

    perf_local.pin_soldr_zccache_source(soldr)

    wanted = perf_local_pins.exact_workspace_pins(perf_local.REPO_ROOT)["kernal-api"]
    assert (
        manifest.read_text(encoding="utf-8")
        == f'[dependencies]\nkernal-api = "={wanted}"\n'
    )
