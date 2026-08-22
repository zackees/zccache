from ci import compile_boundary, incremental_build


def test_parse_rebuilt_packages_ignores_fresh_and_non_json_lines():
    output = "\n".join(
        [
            "Compiling example",
            '{"reason":"compiler-artifact","package_id":"path+file:///repo/crates/zccache-daemon-core#1.0.0","manifest_path":"/repo/crates/zccache-daemon-core/Cargo.toml","fresh":false}',
            '{"reason":"compiler-artifact","package_id":"path+file:///repo#zccache@1.0.0","fresh":true}',
        ]
    )

    assert incremental_build.parse_rebuilt_packages(output) == ["zccache-daemon-core"]


def test_summarize_reports_distribution_and_union():
    samples = [
        incremental_build.BuildSample("compile", 1, 10, 100, ["a"], "a.rs"),
        incremental_build.BuildSample("compile", 2, 20, 200, ["a", "b"], "a.rs"),
        incremental_build.BuildSample("compile", 3, 30, 150, ["b"], "a.rs"),
    ]

    assert incremental_build.summarize(samples) == {
        "samples": 3,
        "min_ns": 10,
        "median_ns": 20,
        "max_ns": 30,
        "mad_ns": 10,
        "peak_rss_bytes": 200,
        "rebuilt_packages": ["a", "b"],
    }


def test_compile_boundary_rejects_new_glob_import(tmp_path):
    server = tmp_path / compile_boundary.SERVER
    subtree = server / "handle_compile"
    subtree.mkdir(parents=True)
    for name in compile_boundary.COMPILE_ROOT_FILES:
        (server / name).write_text("", encoding="utf-8")
    (subtree / "new_stage.rs").write_text("use super::*;\n", encoding="utf-8")
    (server / "helper.rs").write_text("pub(super) fn private_helper() {}\n", encoding="utf-8")

    result = compile_boundary.inventory(tmp_path)

    assert result["unexpected_glob_imports"] == ["handle_compile/new_stage.rs"]


def test_compile_boundary_reports_private_symbol_references(tmp_path):
    server = tmp_path / compile_boundary.SERVER
    subtree = server / "handle_compile"
    subtree.mkdir(parents=True)
    for name in compile_boundary.COMPILE_ROOT_FILES:
        (server / name).write_text("", encoding="utf-8")
    (subtree / "stage.rs").write_text("fn call() { private_helper(); }\n", encoding="utf-8")
    (server / "helper.rs").write_text("pub(super) fn private_helper() {}\n", encoding="utf-8")

    result = compile_boundary.inventory(tmp_path)

    assert result["server_private_references"] == {"private_helper": ["handle_compile/stage.rs"]}


def test_module_private_items_excludes_impl_methods():
    source = """
pub(super) struct SharedState {}
impl SharedState {
    pub(super) fn new() -> Self { Self {} }
}
pub(super) fn module_helper() {}
"""

    assert compile_boundary.module_private_items(source) == {
        "SharedState",
        "module_helper",
    }


def test_repository_compile_boundary_does_not_add_glob_imports():
    result = compile_boundary.inventory(compile_boundary.ROOT)

    assert result["unexpected_glob_imports"] == []


def test_a_test_module_glob_is_not_compile_coupling(tmp_path):
    """`use super::*;` inside `#[cfg(test)] mod tests` is the ordinary Rust
    idiom for a module reaching its own items. Flagging it forced test-only
    files onto the allowlist, which then exempted them from the real check."""
    server = tmp_path / compile_boundary.SERVER
    (server / "handle_compile").mkdir(parents=True)
    for name in compile_boundary.COMPILE_ROOT_FILES:
        (server / name).write_text("fn placeholder() {}\n", encoding="utf-8")
    (server / "handle_compile" / "pipeline.rs").write_text(
        "fn run() {}\n"
        "\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    use super::*;\n"
        "\n"
        "    #[test]\n"
        "    fn it_runs() { run(); }\n"
        "}\n",
        encoding="utf-8",
    )

    result = compile_boundary.inventory(tmp_path)

    assert "handle_compile/pipeline.rs" not in result["glob_imports"]
    assert result["unexpected_glob_imports"] == []


def test_a_production_glob_is_still_rejected(tmp_path):
    """The ratchet must survive the fix above: a top-level glob in a file that
    also has a test module is still coupling."""
    server = tmp_path / compile_boundary.SERVER
    (server / "handle_compile").mkdir(parents=True)
    for name in compile_boundary.COMPILE_ROOT_FILES:
        (server / name).write_text("fn placeholder() {}\n", encoding="utf-8")
    (server / "handle_compile" / "pipeline.rs").write_text(
        "use super::*;\n"
        "\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    use super::*;\n"
        "}\n",
        encoding="utf-8",
    )

    result = compile_boundary.inventory(tmp_path)

    assert result["unexpected_glob_imports"] == ["handle_compile/pipeline.rs"]


def test_a_cfg_test_path_module_is_treated_as_test_code(tmp_path):
    """A whole file included as `#[cfg(test)] #[path = "x.rs"] mod` is test
    code, so its top-level glob is a test glob. Detected structurally, not by
    a `*_tests.rs` naming convention."""
    server = tmp_path / compile_boundary.SERVER
    (server / "handle_compile").mkdir(parents=True)
    for name in compile_boundary.COMPILE_ROOT_FILES:
        (server / name).write_text("fn placeholder() {}\n", encoding="utf-8")
    (server / "handle_compile" / "store.rs").write_text(
        "fn run() {}\n"
        "\n"
        '#[cfg(test)]\n'
        '#[path = "store_tests.rs"]\n'
        "mod store_tests;\n",
        encoding="utf-8",
    )
    (server / "handle_compile" / "store_tests.rs").write_text(
        "use crate::daemon::server::*;\n\n#[test]\nfn it_runs() {}\n",
        encoding="utf-8",
    )

    result = compile_boundary.inventory(tmp_path)

    assert result["unexpected_glob_imports"] == []


def test_strip_test_items_leaves_production_code_intact():
    source = (
        "use super::*;\n"
        "fn run() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn helper() { let _ = \"}\"; }\n"
        "}\n"
        "fn after() {}\n"
    )

    stripped = compile_boundary.strip_test_items(source)

    assert "use super::*;" in stripped
    assert "fn run()" in stripped
    assert "fn after()" in stripped, "code after a test module must survive"
    assert "fn helper()" not in stripped


def test_allowlist_has_no_dead_entries():
    """An allowlist entry that no longer matches a real production glob
    silently exempts that file from the check. That is exactly how three
    test-only files came to be listed."""
    result = compile_boundary.inventory(compile_boundary.ROOT)

    dead = sorted(set(result["allowed_glob_imports"]) - set(result["glob_imports"]))

    assert dead == [], f"allowlist entries with no production glob: {dead}"
