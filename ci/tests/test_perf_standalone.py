import json
import subprocess
from argparse import Namespace

import pytest

from ci import benchmark_stats, perf_sample_monitor, perf_standalone


def test_campaign_inventory_matches_registered_benchmarks():
    expected = [
        (language, test_name)
        for language in benchmark_stats.LANGUAGES
        for test_name in benchmark_stats.BENCHMARK_TESTS_BY_LANGUAGE[language]
    ]

    assert perf_standalone.campaign_inventory() == expected


def test_default_campaign_paths_separate_diagnostics_and_attempt_counts():
    diagnostic = perf_standalone.default_campaign_dir(
        "abcdef1234567890", "c++", "perf_response_file", 1
    )
    full = perf_standalone.default_campaign_dir(
        "abcdef1234567890", None, None, 5
    )

    assert diagnostic.name == "abcdef123456-c-perf-response-file-a1"
    assert full.name == "abcdef123456-full-a5"


def test_docker_recipe_hash_covers_every_image_input():
    original = perf_standalone.recipe_sha256()
    assert len(original) == 64
    command = perf_standalone.docker_build_command()

    assert "--build-arg" in command
    assert f"CAMPAIGN_RECIPE_SHA={original}" in command
    assert set(perf_standalone.RECIPE_FILES) == {
        perf_standalone.DOCKERFILE,
        perf_standalone.REPO_ROOT / "ci/docker/standalone_perf_entrypoint.sh",
        perf_standalone.REPO_ROOT / "rust-toolchain.toml",
    }


def test_docker_command_uses_read_only_source_and_named_build_volumes(tmp_path):
    command = perf_standalone.docker_run_command(
        repo_root=tmp_path / "source",
        results_dir=tmp_path / "results",
        container_name="campaign-test",
        entrypoint_args=["verify"],
    )
    joined = " ".join(command)

    assert f"type=bind,src={tmp_path / 'source'},dst=/src,readonly" in joined
    assert f"type=bind,src={tmp_path / 'results'},dst=/results" in joined
    assert (
        f"type=bind,src={tmp_path / 'results' / 'fixture'},dst=/artifacts,readonly"
        in joined
    )
    for volume, destination in perf_standalone.BUILD_VOLUMES.items():
        assert f"type=volume,src={volume},dst={destination}" in joined
    assert "zccache-standalone-artifacts" not in joined

    build = " ".join(perf_standalone._benchmark_build_command(tmp_path / "results"))
    fixture_mount = (
        f"type=bind,src={tmp_path / 'results' / 'fixture'},dst=/artifacts"
    )
    assert fixture_mount in build
    assert f"{fixture_mount},readonly" not in build


def test_pinned_tool_versions_are_required():
    versions = {
        "rustc": "rustc 1.95.0 (fake)",
        "clang": "Ubuntu clang version 14.0.0",
        "sccache": "sccache 0.10.0",
        "emscripten": "emcc 3.1.74",
        "soldr": "soldr 0.8.16",
    }

    perf_standalone.validate_tool_versions(versions)

    for tool in versions:
        broken = dict(versions)
        broken[tool] = "missing or wrong"
        with pytest.raises(ValueError, match=tool):
            perf_standalone.validate_tool_versions(broken)


def test_resume_identity_rejects_commit_image_host_or_inventory_changes():
    identity = {
        "commit": "abc123",
        "ref": "perf/example",
        "image_digest": "sha256:image",
        "host_fingerprint": "host-a",
        "inventory": [["c", "perf_c_zccache_vs_bare"]],
        "attempts": 5,
        "fixture": {
            "artifacts": {
                "perf_bench_test": "a" * 64,
                "zccache-ci": "b" * 64,
            }
        },
    }

    perf_standalone.validate_resume_identity(identity, dict(identity))

    for field in identity:
        changed = json.loads(json.dumps(identity))
        changed[field] = "different"
        with pytest.raises(ValueError, match=field):
            perf_standalone.validate_resume_identity(identity, changed)


def test_resume_reuses_recorded_fixture_without_rebuilding(tmp_path, monkeypatch):
    commit = "a" * 40
    fixture_hashes = {
        "perf_bench_test": "b" * 64,
        "zccache-ci": "c" * 64,
    }
    identity = {
        "commit": commit,
        "ref": "fix/perf-resume",
        "dirty": False,
        "image_digest": "sha256:image",
        "host_fingerprint": "host-a",
        "inventory": [],
        "attempts": 5,
        "fixture": {"artifacts": fixture_hashes},
    }
    (tmp_path / "campaign-identity.json").write_text(
        json.dumps(identity), encoding="utf-8"
    )

    monkeypatch.setattr(perf_standalone, "_ensure_clean_checkout", lambda: commit)
    monkeypatch.setattr(perf_standalone, "_selected_inventory", lambda *_: [])
    monkeypatch.setattr(perf_standalone, "_build_image", lambda *_: None)
    monkeypatch.setattr(perf_standalone, "_ensure_volumes", lambda: None)
    monkeypatch.setattr(
        perf_standalone,
        "_build_benchmark",
        lambda *_: pytest.fail("resume rebuilt the recorded benchmark fixture"),
    )
    monkeypatch.setattr(
        perf_standalone,
        "_tool_versions",
        lambda *_: ({"rustc": "pinned"}, fixture_hashes, ["docker", "verify"]),
    )
    monkeypatch.setattr(
        perf_standalone,
        "_host_identity",
        lambda: {"fingerprint": "host-a"},
    )
    monkeypatch.setattr(perf_standalone, "_image_digest", lambda: "sha256:image")
    monkeypatch.setattr(perf_standalone, "_git_output", lambda *_: "fix/perf-resume")

    result = perf_standalone.run_campaign(
        Namespace(
            language=None,
            test=None,
            attempts=5,
            output_dir=tmp_path,
            resume=True,
            rebuild_image=False,
        )
    )

    assert result == tmp_path

    tampered_hashes = dict(fixture_hashes)
    tampered_hashes["zccache-ci"] = "d" * 64
    monkeypatch.setattr(
        perf_standalone,
        "_tool_versions",
        lambda *_: ({"rustc": "pinned"}, tampered_hashes, ["docker", "verify"]),
    )
    monkeypatch.setattr(
        perf_standalone,
        "_run_sample",
        lambda *_: pytest.fail("resume sampled with a tampered fixture"),
    )

    with pytest.raises(ValueError, match="resume identity mismatch for fixture"):
        perf_standalone.run_campaign(
            Namespace(
                language=None,
                test=None,
                attempts=5,
                output_dir=tmp_path,
                resume=True,
                rebuild_image=False,
            )
        )


def test_verify_requires_hashes_for_every_reused_executable(tmp_path, monkeypatch):
    valid_versions = {
        "rustc": "rustc 1.95.0 (fake)",
        "clang": "Ubuntu clang version 14.0.0",
        "sccache": "sccache 0.10.0",
        "emscripten": "emcc 3.1.74",
        "soldr": "soldr 0.8.16",
        "benchmark_sha256": "a" * 64,
        "zccache_ci_sha256": "b" * 64,
    }

    monkeypatch.setattr(
        perf_standalone,
        "_run",
        lambda *_args, **_kwargs: Namespace(stdout=json.dumps(valid_versions)),
    )
    versions, fixture_hashes, command = perf_standalone._tool_versions(tmp_path)

    assert "benchmark_sha256" not in versions
    assert "zccache_ci_sha256" not in versions
    assert fixture_hashes == {
        "perf_bench_test": "a" * 64,
        "zccache-ci": "b" * 64,
    }
    assert (
        f"type=bind,src={tmp_path / 'fixture'},dst=/artifacts,readonly"
        in " ".join(command)
    )
    assert (
        f"type=bind,src={tmp_path},dst=/results,readonly" in " ".join(command)
    )

    for artifact, missing_field in perf_standalone.FIXTURE_HASH_FIELDS.items():
        incomplete = dict(valid_versions)
        incomplete[missing_field] = ""
        monkeypatch.setattr(
            perf_standalone,
            "_run",
            lambda *_args, payload=incomplete, **_kwargs: Namespace(
                stdout=json.dumps(payload)
            ),
        )

        with pytest.raises(ValueError, match=artifact):
            perf_standalone._tool_versions(tmp_path)


def test_resume_requires_recorded_identity_before_container_work(
    tmp_path, monkeypatch
):
    monkeypatch.setattr(perf_standalone, "_ensure_clean_checkout", lambda: "a" * 40)
    monkeypatch.setattr(perf_standalone, "_selected_inventory", lambda *_: [])
    monkeypatch.setattr(
        perf_standalone,
        "_build_image",
        lambda *_: pytest.fail("resume touched Docker without recorded identity"),
    )

    with pytest.raises(RuntimeError, match="cannot resume campaign without"):
        perf_standalone.run_campaign(
            Namespace(
                language=None,
                test=None,
                attempts=5,
                output_dir=tmp_path,
                resume=True,
                rebuild_image=False,
            )
        )


def test_invalid_or_fallback_sample_cannot_enter_campaign():
    valid = {
        "passed": True,
        "attempt_policy": "all-required",
        "attempt_count": 5,
        "command_failures": [],
        "missing_requirements": [],
        "infrastructure": {
            "valid": True,
            "invalid_reasons": [],
            "fallback_count": 0,
            "cache_telemetry": {
                "fallback_count": 0,
                "rows": [
                    {
                        "cache_phase": "warm-hit-path",
                        "cache_bytes_reported": True,
                        "bare_cache_bytes": 0,
                        "sccache_cache_bytes": 1024,
                        "zccache_cache_bytes": 512,
                    }
                ],
            },
        },
        "statuses": [
            {
                "samples": [
                    {
                        "attempt": attempt,
                        "attempt_json": f"attempt-{attempt}.json",
                        "raw_log": f"attempt-{attempt}.log",
                    }
                    for attempt in range(1, 6)
                ]
            }
        ],
    }

    perf_standalone.validate_sample_summary(valid, expected_attempts=5)

    cases = []
    for field, value in (
        ("passed", False),
        ("attempt_count", 4),
        ("command_failures", [2]),
        ("missing_requirements", ["emscripten warm"]),
    ):
        sample = json.loads(json.dumps(valid))
        sample[field] = value
        cases.append(sample)
    fallback = json.loads(json.dumps(valid))
    fallback["infrastructure"]["fallback_count"] = 1
    cases.append(fallback)
    missing_telemetry = json.loads(json.dumps(valid))
    missing_telemetry["infrastructure"].pop("cache_telemetry")
    cases.append(missing_telemetry)
    incomplete = json.loads(json.dumps(valid))
    incomplete["statuses"][0]["samples"].pop()
    cases.append(incomplete)

    for sample in cases:
        with pytest.raises(ValueError):
            perf_standalone.validate_sample_summary(sample, expected_attempts=5)


def test_busy_host_detection_uses_competing_container_cpu_and_process_names():
    assert perf_standalone.HOST_PROCESS_ENUMERATION_TIMEOUT_SECONDS >= 30

    assert not perf_standalone.busy_reasons(
        containers=[("soldr-perf-local", 0.02)],
        process_names=[],
    )
    assert perf_standalone.busy_reasons(
        containers=[("soldr-perf-local", 12.5)],
        process_names=[],
    ) == ["container soldr-perf-local is using 12.50% CPU"]
    assert perf_standalone.busy_reasons(
        containers=[],
        process_names=["perf_bench_test"],
    ) == ["host process perf_bench_test is active"]


def test_windows_process_activity_ignores_only_idle_orphan_rustc():
    first = {
        10: ("rustc", 20.0),
        20: ("unrelated", 5.0),
    }
    second = dict(first)

    assert perf_standalone.active_windows_process_names(first, second) == []

    second[10] = ("rustc", 20.5)
    assert perf_standalone.active_windows_process_names(first, second, 1.0) == []

    second[10] = ("rustc", 21.0)
    assert perf_standalone.active_windows_process_names(first, second, 1.0) == [
        "rustc"
    ]

    second[10] = ("rustc", 20.0)
    second[30] = ("cargo", 2.0)
    assert perf_standalone.active_windows_process_names(first, second) == [
        "rustc",
        "cargo",
    ]


def test_sample_monitor_rejects_activity_that_starts_after_launch():
    class Process:
        waits = 0

        def wait(self, timeout):
            assert timeout == perf_sample_monitor.SAMPLE_MONITOR_INTERVAL_SECONDS
            self.waits += 1
            if self.waits == 1:
                raise subprocess.TimeoutExpired("sample", timeout)
            return 0

    process_samples = iter([[], ["perf_bench_test"]])
    return_code, reasons = perf_sample_monitor.monitor_sample_process(
        Process(),
        "zccache-standalone-own-sample",
        lambda _finished: next(process_samples),
        lambda: [("zccache-standalone-own-sample", 90.0)],
        perf_standalone.busy_reasons,
    )

    assert return_code == 0
    assert reasons == ["host process perf_bench_test is active"]


def test_linux_monitor_excludes_the_sample_container_process_tree():
    processes = [
        (101, 1, "perf_bench_test"),
        (102, 101, "em++"),
        (103, 102, "clang"),
        (201, 1, "sccache"),
    ]

    assert perf_sample_monitor.process_names_outside_container(
        processes, {101}, {101}
    ) == ["sccache"]


def test_sample_monitor_records_probe_failure_and_reaps_process():
    class Process:
        waits = 0

        def wait(self, timeout):
            self.waits += 1
            if self.waits == 1:
                raise subprocess.TimeoutExpired("sample", timeout)
            return 0

    process = Process()

    def failed_probe(_finished):
        raise RuntimeError("process enumeration unavailable")

    return_code, reasons = perf_sample_monitor.monitor_sample_process(
        process,
        "zccache-standalone-own-sample",
        failed_probe,
        lambda: [],
        perf_standalone.busy_reasons,
    )

    assert return_code == 0
    assert process.waits == 2
    assert reasons == [
        "host process monitor failed: process enumeration unavailable"
    ]


def test_no_summary_contamination_is_quarantined_before_retry(tmp_path):
    sample_dir = tmp_path / "sample"
    sample_dir.mkdir()
    evidence = sample_dir / "attempt-1.log"
    evidence.write_bytes(b"contaminated evidence")

    with pytest.raises(RuntimeError, match="evidence retained"):
        perf_standalone._reject_contaminated_sample(
            sample_dir, ["host process perf_bench_test is active"]
        )
    quarantine, = tmp_path.glob("sample.contaminated-*")
    sample_dir.mkdir()
    (sample_dir / "attempt-1.log").write_bytes(b"clean retry")

    assert (quarantine / "attempt-1.log").read_bytes() == b"contaminated evidence"
    invalid = json.loads(
        (quarantine / "infrastructure-invalid.json").read_text(encoding="utf-8")
    )
    assert invalid["valid"] is False
    assert evidence.read_bytes() == b"clean retry"


def test_host_fingerprint_input_ignores_dynamic_docker_state():
    first = {
        "ID": "host-id",
        "Name": "docker-desktop",
        "ServerVersion": "28.5.1",
        "NCPU": 8,
        "MemTotal": 8_000_000,
        "ContainersRunning": 1,
        "SystemTime": "first",
    }
    second = dict(first, ContainersRunning=9, SystemTime="second")

    assert perf_standalone.stable_docker_identity(
        first
    ) == perf_standalone.stable_docker_identity(second)

    second["NCPU"] = 16
    assert perf_standalone.stable_docker_identity(
        first
    ) != perf_standalone.stable_docker_identity(second)


def test_dockerfile_pins_all_campaign_tools():
    dockerfile = (
        perf_standalone.REPO_ROOT / "ci/docker/standalone-perf.Dockerfile"
    ).read_text(encoding="utf-8")

    assert "emscripten/emsdk:3.1.74@sha256:" in dockerfile
    assert "SOLDR_VERSION=0.8.16" in dockerfile
    assert "SCCACHE_VERSION=0.10.0" in dockerfile
    assert "RUST_VERSION=1.95.0" in dockerfile
    assert "clang-14" in dockerfile
    assert 'org.zccache.campaign.recipe="${CAMPAIGN_RECIPE_SHA}"' in dockerfile
    assert "SOLDR_COMMAND_OUTPUT_TIMEOUT_SECS=600" in dockerfile
    assert "HOME=/opt/soldr-seed soldr toolchain prepare" in dockerfile
    assert "SOLDR_HOME=" not in dockerfile
    assert "https://sh.rustup.rs" not in dockerfile
    assert "/opt/rust" not in dockerfile


def test_runtime_rust_state_is_owned_and_seeded_by_soldr():
    assert perf_standalone.BUILD_VOLUMES[
        "zccache-standalone-soldr-home"
    ] == "/root/.soldr"
    assert all(
        destination not in {"/cargo-home", "/rustup-home"}
        for destination in perf_standalone.BUILD_VOLUMES.values()
    )

    entrypoint = (
        perf_standalone.REPO_ROOT / "ci/docker/standalone_perf_entrypoint.sh"
    ).read_text(encoding="utf-8")
    assert "cp -a /opt/soldr-seed/.soldr/." in entrypoint
    assert '.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}' in entrypoint
    assert 'find "${soldr_root}" -mindepth 1 -maxdepth 1' in entrypoint
    assert entrypoint.index('cp -a /opt/soldr-seed/.soldr/.') < entrypoint.index(
        'touch "${soldr_root}/${sentinel}"'
    )
    assert 'export CARGO_HOME="${soldr_root}/cargo"' in entrypoint
    assert 'export RUSTUP_HOME="${soldr_root}/rustup"' in entrypoint
    assert "/opt/rust" not in entrypoint


def test_standalone_run_builds_and_wires_log_auditor():
    entrypoint = (
        perf_standalone.REPO_ROOT / "ci/docker/standalone_perf_entrypoint.sh"
    ).read_text(encoding="utf-8")

    assert "soldr cargo build -p zccache --features ci-bin --bin zccache-ci --release" in entrypoint
    assert "install -m 0755 /target/release/zccache-ci /artifacts/zccache-ci" in entrypoint
    assert "require_mount /artifacts/zccache-ci" in entrypoint
    assert "export ZCCACHE_CI_BIN=/artifacts/zccache-ci" in entrypoint


def test_campaign_artifact_paths_are_relative_and_complete(tmp_path):
    sample_dir = tmp_path / "c" / "perf_c_zccache_vs_bare"
    sample_dir.mkdir(parents=True)
    for name in (
        "perf-guard-summary.json",
        "perf-guard-summary.md",
        "perf-guard-result.txt",
        "resource-usage.txt",
        "attempt-1.json",
        "attempt-1.log",
    ):
        (sample_dir / name).touch()

    paths = perf_standalone.artifact_paths(tmp_path, sample_dir)

    assert paths["summary_json"] == "c/perf_c_zccache_vs_bare/perf-guard-summary.json"
    assert paths["raw_logs"] == ["c/perf_c_zccache_vs_bare/attempt-1.log"]
    assert paths["attempt_json"] == ["c/perf_c_zccache_vs_bare/attempt-1.json"]


def test_sample_telemetry_retains_cache_phase_and_byte_counts(tmp_path):
    attempt = {
        "attempt": 1,
        "rows": [
            {
                "benchmark": "c",
                "scenario": "cold compile",
                "mode": "cold",
                "cache_bytes_reported": True,
                "bare_cache_bytes": 0,
                "sccache_cache_bytes": 1024,
                "zccache_cache_bytes": 512,
            },
            {
                "benchmark": "c",
                "scenario": "warm compile",
                "mode": "warm",
                "cache_bytes_reported": True,
                "bare_cache_bytes": 0,
                "sccache_cache_bytes": 1024,
                "zccache_cache_bytes": 512,
            },
        ],
    }
    (tmp_path / "attempt-1.json").write_text(json.dumps(attempt), encoding="utf-8")

    telemetry = perf_standalone._sample_cache_telemetry(tmp_path)

    assert telemetry["cold_miss_path_rows"] == 1
    assert telemetry["warm_hit_path_rows"] == 1
    assert telemetry["fallback_count"] == 0
    assert telemetry["rows"][1]["zccache_cache_bytes"] == 512


def test_summary_metadata_is_corrected_to_campaign_identity(tmp_path):
    summary_path = tmp_path / "perf-guard-summary.json"
    summary_path.write_text(
        json.dumps(
            {
                "metadata": {"git_sha": None, "dirty": True},
                "command_failures": [],
                "missing_requirements": [],
            }
        ),
        encoding="utf-8",
    )
    identity = {
        "commit": "abc123",
        "ref": "perf/example",
        "dirty": False,
        "image_digest": "sha256:image",
        "host_fingerprint": "host-a",
    }
    command = ["docker", "run", "image"]
    telemetry = {
        "fallback_count": 0,
        "rows": [{"cache_phase": "warm-hit-path"}],
    }

    enriched = perf_standalone._enrich_summary(
        summary_path, 0, identity, command, telemetry
    )

    assert enriched["metadata"]["git_sha"] == "abc123"
    assert enriched["metadata"]["git_ref"] == "perf/example"
    assert enriched["metadata"]["dirty"] is False
    assert enriched["metadata"]["docker_command"] == command
    assert enriched["infrastructure"]["cache_telemetry"] == telemetry


def test_resume_revalidates_completed_sample_artifacts(tmp_path):
    sample_dir = tmp_path / "c" / "perf_c_zccache_vs_bare"
    sample_dir.mkdir(parents=True)
    valid = {
        "passed": True,
        "attempt_policy": "all-required",
        "attempt_count": 1,
        "command_failures": [],
        "missing_requirements": [],
        "infrastructure": {
            "valid": True,
            "invalid_reasons": [],
            "fallback_count": 0,
            "cache_telemetry": {
                "fallback_count": 0,
                "rows": [
                    {
                        "cache_phase": "warm-hit-path",
                        "cache_bytes_reported": True,
                        "bare_cache_bytes": 0,
                        "sccache_cache_bytes": 1,
                        "zccache_cache_bytes": 1,
                    }
                ],
            },
        },
        "statuses": [
            {
                "samples": [
                    {
                        "attempt_json": "attempt-1.json",
                        "raw_log": "attempt-1.log",
                    }
                ]
            }
        ],
    }
    files = {
        "perf-guard-summary.json": json.dumps(valid),
        "perf-guard-summary.md": "summary",
        "perf-guard-result.txt": "pass",
        "resource-usage.txt": "rss",
        "attempt-1.json": "{}",
        "attempt-1.log": "log",
    }
    for name, content in files.items():
        (sample_dir / name).write_text(content, encoding="utf-8")
    campaign = {
        "results": [
            {
                "artifacts": perf_standalone.artifact_paths(tmp_path, sample_dir)
            }
        ]
    }

    perf_standalone.validate_completed_results(tmp_path, campaign, 1)

    (sample_dir / "attempt-1.log").unlink()
    with pytest.raises(ValueError, match="missing artifacts"):
        perf_standalone.validate_completed_results(tmp_path, campaign, 1)
