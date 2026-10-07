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


def test_pin_soldr_source_aligns_locked_transitive_exact_pin(
    tmp_path: Path, monkeypatch
) -> None:
    """#1935: kernal-api's running-process pin must resolve with patched zccache."""
    checkout = tmp_path / "zccache"
    checkout.mkdir()
    (checkout / "Cargo.toml").write_text(
        '[workspace.package]\nversion = "1.0.0"\n'
        '[workspace.dependencies]\nkernal-api = "=0.1.26"\n',
        encoding="utf-8",
    )
    (checkout / "Cargo.lock").write_text(
        '[[package]]\nname = "running-process"\nversion = "4.10.16"\n'
        'source = "registry+https://github.com/rust-lang/crates.io-index"\n',
        encoding="utf-8",
    )
    soldr = tmp_path / "soldr"
    soldr.mkdir()
    manifest = soldr / "Cargo.toml"
    manifest.write_text(
        '[dependencies]\nrunning-process = "=4.10.14"\n', encoding="utf-8"
    )
    monkeypatch.setattr(perf_local, "REPO_ROOT", checkout)
    monkeypatch.setattr(perf_local, "git_is_dirty", lambda _repo: False)

    perf_local.pin_soldr_zccache_source(soldr)

    assert 'running-process = "=4.10.16"' in manifest.read_text(encoding="utf-8")


def test_checkout_pins_exclude_ambiguous_and_nonregistry_packages(
    tmp_path: Path,
) -> None:
    (tmp_path / "Cargo.toml").write_text(
        '[workspace.dependencies]\nexplicit = "=2.0.0"\n', encoding="utf-8"
    )
    entries = [
        ("explicit", "1.0.0", "registry+https://example.test/index"),
        ("explicit", "2.0.0", "registry+https://example.test/index"),
        ("ambiguous", "1.0.0", "registry+https://example.test/index"),
        ("ambiguous", "2.0.0", "registry+https://example.test/index"),
        ("mixed-source", "1.0.0", "registry+https://example.test/index"),
        ("mixed-source", "1.0.0", "git+https://example.test/repo"),
        ("git-only", "1.0.0", "git+https://example.test/repo"),
        ("path-only", "1.0.0", None),
    ]
    (tmp_path / "Cargo.lock").write_text(
        "".join(
            f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
            + (f'source = "{source}"\n' if source else "")
            for name, version, source in entries
        ),
        encoding="utf-8",
    )
    assert perf_local_pins.exact_checkout_pins(tmp_path) == {"explicit": "2.0.0"}
