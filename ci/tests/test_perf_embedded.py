"""Contracts for the soldr-embedded mixed-language campaign (#1117)."""

from __future__ import annotations

from pathlib import Path

from ci import perf_embedded


def _phase(
    name: str,
    *,
    compilations: int,
    hits: int,
    misses: int,
    artifact_sha256: str = "a" * 64,
) -> dict:
    return {
        "name": name,
        "wall_ms": 100,
        "user_cpu_ms": 40,
        "system_cpu_ms": 10,
        "ttfb_ms": 5,
        "output_bytes": 20,
        "peak_command_rss_bytes": 4096,
        "cache_bytes": 1024,
        "artifact_count": 2,
        "artifact_bytes": 512,
        "artifact_sha256": artifact_sha256,
        "compilations": compilations,
        "hits": hits,
        "misses": misses,
        "non_cacheable": 0,
        "phase_profile": {"hit_count": hits, "miss_count": misses},
        "command": ["soldr", "cargo", "build", "--release"],
        "working_directory": "/tmp/fixture",
        "artifacts": {
            "command_log": f"phase-{name}.log",
            "resource_usage": f"phase-{name}-resources.txt",
            "cache_report": f"phase-{name}-cache-report.json",
            "output_manifest": f"phase-{name}-outputs.json",
        },
    }


def _result(language: str) -> dict:
    phases = {
        "daemon-start": _phase("daemon-start", compilations=0, hits=0, misses=0),
        "daemon-cold": _phase("daemon-cold", compilations=4, hits=0, misses=4),
        "local-hit": _phase("local-hit", compilations=4, hits=4, misses=0),
        "sibling-hit": _phase("sibling-hit", compilations=4, hits=4, misses=0),
        "target-noop": _phase("target-noop", compilations=0, hits=0, misses=0),
    }
    return {
        "schema_version": 1,
        "language": language,
        "infrastructure_valid": True,
        "invalid_reasons": [],
        "soldr_abort_count": 0,
        "soldr_timeout_count": 0,
        "soldr_no_cache_retry_count": 0,
        "soldr_daemon_fallback_count": 0,
        "soldr_abort_evidence": ["soldr-aborts-all.jsonl"],
        "soldr_daemon_fallback_evidence": ["soldr-daemon-fallbacks-all.jsonl"],
        "compiler_command": ("rustc" if language == "rust" else f"/root/.soldr/bin/zccache-soldr /usr/bin/{language}"),
        "fixture_sha256": "b" * 64,
        "embedded_zccache_observed": True,
        "soldr_sha": "c" * 40,
        "zccache_sha": "d" * 40,
        "image_digest": "sha256:" + "e" * 64,
        "host_fingerprint": "f" * 64,
        "tool_versions": {
            "soldr": "soldr 0.8.16",
            "rustc": "rustc 1.94.1",
            "clang": "clang version 14.0.0",
            "emscripten": "3.1.74",
        },
        "peak_daemon_rss_bytes": 1000,
        "peak_compile_rss_bytes": 2000,
        "phases": phases,
    }


def _write_artifacts(results_dir: Path, result: dict) -> None:
    for phase in result["phases"].values():
        for relative in phase["artifacts"].values():
            path = results_dir / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            if relative.endswith(".json"):
                path.write_text("{}", encoding="utf-8")
            else:
                path.write_text("evidence", encoding="utf-8")
    for relative in result["soldr_abort_evidence"] + result["soldr_daemon_fallback_evidence"]:
        (results_dir / relative).write_text("", encoding="utf-8")


def test_inventory_covers_every_language_and_lifecycle() -> None:
    assert perf_embedded.LANGUAGES == ("rust", "c", "cpp", "emscripten")
    assert perf_embedded.LIFECYCLES == (
        "daemon-start",
        "daemon-cold",
        "local-hit",
        "sibling-hit",
        "target-noop",
    )


def test_complete_sample_passes_strict_validation(tmp_path: Path) -> None:
    result = _result("cpp")
    _write_artifacts(tmp_path, result)

    assert perf_embedded.validate_embedded_result(result, tmp_path) == []


def test_native_sample_requires_soldr_zccache_wrapper(tmp_path: Path) -> None:
    result = _result("emscripten")
    result["compiler_command"] = "/emsdk/upstream/emscripten/em++"
    _write_artifacts(tmp_path, result)

    failures = perf_embedded.validate_embedded_result(result, tmp_path)

    assert "compiler command bypassed zccache-soldr" in failures


def test_sample_rejects_fallback_and_incomplete_lifecycle(tmp_path: Path) -> None:
    result = _result("c")
    result["soldr_daemon_fallback_count"] = 1
    result["phases"].pop("sibling-hit")
    _write_artifacts(tmp_path, result)

    failures = perf_embedded.validate_embedded_result(result, tmp_path)

    assert any("fallback" in failure for failure in failures)
    assert any("lifecycle" in failure for failure in failures)


def test_sample_requires_embedded_zccache_observation(tmp_path: Path) -> None:
    result = _result("rust")
    result["embedded_zccache_observed"] = False
    _write_artifacts(tmp_path, result)

    failures = perf_embedded.validate_embedded_result(result, tmp_path)

    assert any("embedded zccache invocation" in failure for failure in failures)


def test_sample_rejects_wrong_cache_semantics_and_artifact_drift(
    tmp_path: Path,
) -> None:
    result = _result("cpp")
    result["phases"]["local-hit"]["hits"] = 0
    result["phases"]["sibling-hit"]["artifact_sha256"] = "9" * 64
    result["phases"]["target-noop"]["compilations"] = 1
    _write_artifacts(tmp_path, result)

    failures = perf_embedded.validate_embedded_result(result, tmp_path)

    assert any("local-hit recorded no cache hits" in failure for failure in failures)
    assert any("artifact hashes differ" in failure for failure in failures)
    assert any("target-noop compiled" in failure for failure in failures)


def test_repeat_summary_has_required_distribution_and_floor_dossiers(
    tmp_path: Path,
) -> None:
    samples = []
    for index, wall_ms in enumerate((100, 110, 120, 130, 140), start=1):
        result = _result("rust")
        result["phases"]["local-hit"]["wall_ms"] = wall_ms
        sample_dir = tmp_path / f"sample-{index:02d}"
        sample_dir.mkdir()
        samples.append((sample_dir, result))

    summary = perf_embedded.build_repeat_summary("rust", tmp_path, samples)

    distribution = summary["distributions"]["local-hit"]["wall_ms"]
    assert distribution == {
        "count": 5,
        "min": 100,
        "median": 120,
        "mad": 10,
        "max": 140,
    }
    assert summary["floor_dossiers"]["local-hit"]["observed_floor_ms"] == 100


def test_campaign_resume_identity_rejects_source_or_fixture_drift() -> None:
    existing = {
        "zccache_sha": "a",
        "soldr_sha": "b",
        "image_digest": "c",
        "host_fingerprint": "d",
        "fixture_sha256": "e",
        "repeat": 5,
    }
    current = dict(existing, fixture_sha256="changed")

    try:
        perf_embedded.validate_resume_identity(existing, current)
    except ValueError as error:
        assert "fixture_sha256" in str(error)
    else:
        raise AssertionError("fixture drift must reject resume")


def test_fixture_hash_changes_with_content(tmp_path: Path) -> None:
    (tmp_path / "Cargo.toml").write_text("[package]\nname='fixture'\n")
    first = perf_embedded.fixture_sha256(tmp_path)
    (tmp_path / "Cargo.toml").write_text("[package]\nname='changed'\n")

    assert perf_embedded.fixture_sha256(tmp_path) != first


def test_perf_local_exposes_embedded_matrix_mode() -> None:
    orchestrator = (Path(__file__).resolve().parents[1] / "perf_local.py").read_text(encoding="utf-8")

    assert '"--embedded-matrix"' in orchestrator
    assert "run_embedded_campaign" in orchestrator
    assert "sys.path.insert(0, str(REPO_ROOT))" in orchestrator


def test_embedded_runner_uses_pinned_compiler_complete_base() -> None:
    root = Path(__file__).resolve().parents[2]
    dockerfile = (root / "ci/docker/embedded-perf.Dockerfile").read_text(encoding="utf-8")
    entrypoint = (root / "ci/docker/embedded_perf_entrypoint.sh").read_text(encoding="utf-8")
    scenario = (root / "perf/scenarios/embedded-lifecycle/run.sh").read_text(encoding="utf-8")

    assert "FROM zccache-standalone-perf:1" in dockerfile
    assert "soldr toolchain prepare" not in entrypoint
    assert "cp -a /opt/soldr-seed/.soldr/." in entrypoint
    assert 'sentinel=".embedded-toolchain-${IMAGE_DIGEST#sha256:}"' in entrypoint
    assert "embedded-lifecycle/run.sh" in entrypoint
    assert "compiler-artifact" in scenario
    assert 'target="${repo}/target"' in scenario
    assert 'rust_binary="${target}/release/embedded-fixture"' in scenario


def test_runtime_is_offline_and_uses_exact_soldr_binary(tmp_path: Path) -> None:
    layout = {"bin_soldr": tmp_path / "binaries"}
    identity = {
        "fixture_sha256": "a" * 64,
        "soldr_sha": "b" * 40,
        "zccache_sha": "c" * 40,
        "image_digest": "sha256:" + "d" * 64,
        "host_fingerprint": "e" * 64,
    }

    command = perf_embedded._docker_command(
        layout=layout,
        results_dir=tmp_path / "results",
        language="cpp",
        identity=identity,
        jobs=2,
        sample_index=1,
    )
    joined = " ".join(command)

    assert "--network none" in joined
    assert "/usr/local/bin/soldr:ro" in joined
    assert "SOLDR_DAEMON_REQUIRED=1" in joined
    assert f"{perf_embedded.SOLDR_HOME_VOLUME}:/root/.soldr" in joined
