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
