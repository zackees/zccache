"""Soldr-embedded mixed-language performance campaign for issue #1117."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import time
from pathlib import Path
from typing import Any

from ci import perf_local, perf_standalone


REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_ROOT = REPO_ROOT / "perf/fixtures/embedded-mixed"
IMAGE = "zccache-perf-embedded-runner:1"
RESULTS_ROOT = REPO_ROOT / ".perf-local/results/embedded-mixed"
DOCKERFILE = REPO_ROOT / "ci/docker/embedded-perf.Dockerfile"
ENTRYPOINT = REPO_ROOT / "ci/docker/embedded_perf_entrypoint.sh"
SCENARIO = REPO_ROOT / "perf/scenarios/embedded-lifecycle/run.sh"
SOLDR_HOME_VOLUME = "zccache-perf-embedded-soldr-home"
LANGUAGES = ("rust", "c", "cpp", "emscripten")
LIFECYCLES = (
    "daemon-start",
    "daemon-cold",
    "local-hit",
    "sibling-hit",
    "target-noop",
)
RESUME_FIELDS = (
    "zccache_sha",
    "soldr_sha",
    "image_digest",
    "host_fingerprint",
    "fixture_sha256",
    "repeat",
    "languages",
)


def fixture_sha256(root: Path = FIXTURE_ROOT) -> str:
    """Hash fixture paths and bytes while excluding generated state."""
    digest = hashlib.sha256()
    for path in sorted(item for item in root.rglob("*") if item.is_file()):
        relative = path.relative_to(root)
        if any(part in {".git", "target"} for part in relative.parts):
            continue
        digest.update(relative.as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def validate_resume_identity(existing: dict[str, Any], current: dict[str, Any]) -> None:
    for field in RESUME_FIELDS:
        if existing.get(field) != current.get(field):
            raise ValueError(f"embedded campaign resume identity mismatch for {field}: {existing.get(field)!r} != {current.get(field)!r}")


def _is_nonnegative_int(value: Any) -> bool:
    return type(value) is int and value >= 0


def _is_hex(value: Any, length: int) -> bool:
    return isinstance(value, str) and re.fullmatch(rf"[0-9a-f]{{{length}}}", value) is not None


def _validate_phase(
    phase_name: str,
    phase: Any,
    results_dir: Path,
) -> list[str]:
    if not isinstance(phase, dict):
        return [f"{phase_name} phase is not an object"]
    failures: list[str] = []
    required_nonnegative = (
        "wall_ms",
        "user_cpu_ms",
        "system_cpu_ms",
        "ttfb_ms",
        "output_bytes",
        "peak_command_rss_bytes",
        "cache_bytes",
        "artifact_count",
        "artifact_bytes",
        "compilations",
        "hits",
        "misses",
        "non_cacheable",
    )
    for field in required_nonnegative:
        if not _is_nonnegative_int(phase.get(field)):
            failures.append(f"{phase_name} has invalid {field}")
    if phase.get("name") != phase_name:
        failures.append(f"{phase_name} phase name does not match its key")
    if phase.get("wall_ms") == 0:
        failures.append(f"{phase_name} wall time is zero")
    if phase.get("peak_command_rss_bytes") == 0:
        failures.append(f"{phase_name} command RSS is zero")
    wall_ms = phase.get("wall_ms")
    ttfb_ms = phase.get("ttfb_ms")
    if _is_nonnegative_int(wall_ms) and _is_nonnegative_int(ttfb_ms):
        if ttfb_ms > wall_ms:
            failures.append(f"{phase_name} TTFB exceeds wall time")
    command = phase.get("command")
    if not isinstance(command, list) or not all(isinstance(value, str) for value in command):
        failures.append(f"{phase_name} exact command is missing")
    if not isinstance(phase.get("working_directory"), str):
        failures.append(f"{phase_name} working directory is missing")
    if not isinstance(phase.get("phase_profile"), dict):
        failures.append(f"{phase_name} phase profile is missing")
    digest = phase.get("artifact_sha256")
    if not _is_hex(digest, 64):
        failures.append(f"{phase_name} artifact SHA-256 is invalid")
    artifacts = phase.get("artifacts")
    if not isinstance(artifacts, dict):
        failures.append(f"{phase_name} artifact index is missing")
    else:
        required_artifacts = {
            "command_log",
            "resource_usage",
            "cache_report",
            "output_manifest",
        }
        if set(artifacts) != required_artifacts:
            failures.append(f"{phase_name} artifact index is incomplete")
        for relative in artifacts.values():
            if not isinstance(relative, str) or not (results_dir / relative).is_file():
                failures.append(f"{phase_name} evidence is missing: {relative!r}")
    return failures


def validate_embedded_result(result: dict[str, Any], results_dir: Path) -> list[str]:
    """Return every hard-gate failure for one language sample."""
    failures: list[str] = []
    if result.get("schema_version") != 1:
        failures.append("unsupported embedded result schema")
    language = result.get("language")
    if language not in LANGUAGES:
        failures.append(f"unknown embedded language {language!r}")
    try:
        perf_local.validate_infrastructure_result(result, results_dir)
    except ValueError as error:
        failures.append(str(error))

    compiler_command = result.get("compiler_command")
    if language != "rust" and (not isinstance(compiler_command, str) or "zccache-soldr" not in compiler_command):
        failures.append("compiler command bypassed zccache-soldr")
    if result.get("embedded_zccache_observed") is not True:
        failures.append("fixture did not prove an embedded zccache invocation")

    for field, expected_length in (("fixture_sha256", 64), ("host_fingerprint", 64), ("soldr_sha", 40), ("zccache_sha", 40)):
        value = result.get(field)
        if not _is_hex(value, expected_length):
            failures.append(f"invalid {field}")
    image_digest = result.get("image_digest")
    if not isinstance(image_digest, str) or not image_digest.startswith("sha256:") or not _is_hex(image_digest.removeprefix("sha256:"), 64):
        failures.append("invalid image_digest")
    versions = result.get("tool_versions")
    if not isinstance(versions, dict) or not all(isinstance(versions.get(tool), str) and versions[tool].strip() for tool in ("soldr", "rustc", "clang", "emscripten")):
        failures.append("tool version provenance is incomplete")
    for field in ("peak_daemon_rss_bytes", "peak_compile_rss_bytes"):
        if not _is_nonnegative_int(result.get(field)) or result[field] == 0:
            failures.append(f"invalid {field}")

    phases = result.get("phases")
    if not isinstance(phases, dict) or set(phases) != set(LIFECYCLES):
        failures.append("embedded lifecycle inventory is incomplete")
        phases = phases if isinstance(phases, dict) else {}
    for phase_name in LIFECYCLES:
        if phase_name in phases:
            failures.extend(_validate_phase(phase_name, phases[phase_name], results_dir))

    cold = phases.get("daemon-cold", {})
    local_hit = phases.get("local-hit", {})
    sibling_hit = phases.get("sibling-hit", {})
    noop = phases.get("target-noop", {})
    cold_misses = cold.get("misses") if isinstance(cold, dict) else None
    if isinstance(cold, dict) and (cold_misses is None or cold_misses <= 0 or cold.get("hits") != 0):
        failures.append("daemon-cold did not record a pure cache miss")
    for phase_name, phase in (("local-hit", local_hit), ("sibling-hit", sibling_hit)):
        if isinstance(phase, dict) and (phase.get("hits") != cold_misses or phase.get("misses") != 0):
            failures.append(f"{phase_name} did not replay every cold miss as a cache hit")
    if isinstance(noop, dict) and any(noop.get(field) != 0 for field in ("compilations", "hits", "misses", "non_cacheable")):
        failures.append("target-noop performed compiler or cache work")
    for phase_name, phase in (("daemon-cold", cold), ("local-hit", local_hit), ("sibling-hit", sibling_hit), ("target-noop", noop)):
        if isinstance(phase, dict) and (phase.get("artifact_count", 0) <= 0 or phase.get("artifact_bytes", 0) <= 0):
            failures.append(f"{phase_name} recorded no build artifacts")

    comparable = [phase.get("artifact_sha256") for phase in (cold, local_hit, sibling_hit, noop) if isinstance(phase, dict)]
    if comparable and len(set(comparable)) != 1:
        failures.append("artifact hashes differ across lifecycle phases")
    return failures


def _distribution(values: list[int]) -> dict[str, int | float]:
    ordered = sorted(values)
    median = statistics.median(ordered)
    return {
        "count": len(ordered),
        "min": ordered[0],
        "median": median,
        "mad": statistics.median(abs(value - median) for value in ordered),
        "max": ordered[-1],
    }


def build_repeat_summary(
    language: str,
    base_dir: Path,
    samples: list[tuple[Path, dict[str, Any]]],
) -> dict[str, Any]:
    if not samples:
        raise ValueError("embedded repeat summary requires at least one sample")
    distributions: dict[str, dict[str, Any]] = {}
    floor_dossiers: dict[str, dict[str, Any]] = {}
    for lifecycle in LIFECYCLES:
        wall_values = [int(result["phases"][lifecycle]["wall_ms"]) for _, result in samples]
        user_values = [int(result["phases"][lifecycle]["user_cpu_ms"]) for _, result in samples]
        system_values = [int(result["phases"][lifecycle]["system_cpu_ms"]) for _, result in samples]
        distributions[lifecycle] = {
            "wall_ms": _distribution(wall_values),
            "user_cpu_ms": _distribution(user_values),
            "system_cpu_ms": _distribution(system_values),
        }
        floor_dossiers[lifecycle] = {
            "observed_floor_ms": min(wall_values),
            "median_ms": statistics.median(wall_values),
            "mad_ms": statistics.median(abs(value - statistics.median(wall_values)) for value in wall_values),
            "required_work": ("compiler plus zccache miss pipeline" if lifecycle == "daemon-cold" else "soldr front door plus required cache/build-system work"),
            "acceptance": "screening distribution; profile material excess before ratcheting",
        }
    return {
        "schema_version": 1,
        "language": language,
        "samples": [path.relative_to(base_dir).as_posix() for path, _ in samples],
        "distributions": distributions,
        "floor_dossiers": floor_dossiers,
    }


def write_json(path: Path, payload: dict[str, Any]) -> None:
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def recipe_sha256() -> str:
    digest = hashlib.sha256()
    for path in (DOCKERFILE, ENTRYPOINT, SCENARIO):
        digest.update(path.relative_to(REPO_ROOT).as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def _image_digest(image: str = IMAGE) -> str:
    return subprocess.run(
        ["docker", "image", "inspect", "--format", "{{.Id}}", image],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def build_images(*, rebuild: bool) -> list[str]:
    """Ensure the prepared standalone base and embedded runner are current."""
    perf_standalone._build_image(rebuild)
    standalone_digest = _image_digest(perf_standalone.IMAGE)
    inspected = subprocess.run(
        [
            "docker",
            "image",
            "inspect",
            "--format",
            "{{json .Config.Labels}}",
            IMAGE,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    try:
        labels = json.loads(inspected.stdout) if inspected.returncode == 0 else {}
    except json.JSONDecodeError:
        labels = {}
    recipe = recipe_sha256()
    command = [
        "docker",
        "build",
        "--build-arg",
        f"EMBEDDED_RECIPE_SHA={recipe}",
        "--build-arg",
        f"STANDALONE_IMAGE_DIGEST={standalone_digest}",
        "--file",
        str(DOCKERFILE),
        "--tag",
        IMAGE,
        str(REPO_ROOT),
    ]
    if rebuild or labels.get("org.zccache.embedded-perf.recipe") != recipe or labels.get("org.zccache.embedded-perf.standalone-image") != standalone_digest:
        perf_local.run(command)
    return command


def default_campaign_dir(zccache_sha: str, soldr_sha: str, repeat: int, language: str | None) -> Path:
    selection = language or "all"
    return RESULTS_ROOT / (f"{zccache_sha[:12]}-{soldr_sha[:12]}-{selection}-r{repeat}")


def _docker_command(
    *,
    layout: dict[str, Path],
    results_dir: Path,
    language: str,
    identity: dict[str, Any],
    jobs: int,
    sample_index: int,
) -> list[str]:
    soldr_binary = layout["bin_soldr"] / "soldr"
    command = [
        "docker",
        "run",
        "--rm",
        "--network",
        "none",
        "--name",
        f"zccache-embedded-{language}-{sample_index:02d}",
        "-v",
        perf_local.host_volume(soldr_binary, "/usr/local/bin/soldr", "ro"),
        "-v",
        perf_local.host_volume(REPO_ROOT, "/zccache-src", "ro"),
        "-v",
        perf_local.host_volume(results_dir, "/results"),
        "-v",
        f"{SOLDR_HOME_VOLUME}:/root/.soldr",
        "-e",
        f"EMBEDDED_LANGUAGE={language}",
        "-e",
        f"FIXTURE_SHA256={identity['fixture_sha256']}",
        "-e",
        f"SOLDR_SHA={identity['soldr_sha']}",
        "-e",
        f"ZCCACHE_SHA={identity['zccache_sha']}",
        "-e",
        f"IMAGE_DIGEST={identity['image_digest']}",
        "-e",
        f"HOST_FINGERPRINT={identity['host_fingerprint']}",
        "-e",
        f"CARGO_BUILD_JOBS={jobs}",
        "-e",
        "SOLDR_DAEMON_REQUIRED=1",
    ]
    for name, value in os.environ.items():
        if name.startswith("ZCCACHE_"):
            command.extend(["-e", f"{name}={value}"])
    command.append(IMAGE)
    return command


def _load_sample(sample_dir: Path) -> dict[str, Any]:
    result_path = sample_dir / "result.json"
    if not result_path.is_file():
        raise RuntimeError(f"embedded sample did not produce {result_path}")
    result = json.loads(result_path.read_text(encoding="utf-8"))
    failures = validate_embedded_result(result, sample_dir)
    if failures:
        raise ValueError("invalid embedded sample: " + "; ".join(failures))
    return result


def validate_completed_samples(campaign_dir: Path, campaign: dict[str, Any]) -> None:
    if campaign.get("schema_version") != 1 or not isinstance(campaign.get("identity"), dict):
        raise ValueError("embedded campaign metadata is malformed")
    identity = campaign["identity"]
    seen: set[tuple[str, int]] = set()
    for item in campaign.get("samples", []):
        relative = item.get("path")
        language = item.get("language")
        sample = item.get("sample")
        if language not in LANGUAGES or type(sample) is not int or sample < 1:
            raise ValueError("completed embedded sample metadata is malformed")
        expected = f"{language}/sample-{sample:02d}"
        if not isinstance(relative, str) or relative != expected:
            raise ValueError("completed embedded sample has no relative path")
        key = (language, sample)
        if key in seen:
            raise ValueError("completed embedded sample is duplicated")
        seen.add(key)
        sample_dir = campaign_dir / relative
        result = _load_sample(sample_dir)
        if result.get("language") != language:
            raise ValueError("completed embedded sample language drifted")
        for field in ("zccache_sha", "soldr_sha", "image_digest", "host_fingerprint", "fixture_sha256"):
            if result.get(field) != identity.get(field):
                raise ValueError(f"completed embedded sample {field} drifted")


def _render_markdown(campaign: dict[str, Any]) -> str:
    identity = campaign["identity"]
    lines = [
        "# Soldr-embedded Linux Docker performance campaign",
        "",
        f"- zccache: `{identity['zccache_sha']}`",
        f"- soldr: `{identity['soldr_sha']}`",
        f"- image: `{identity['image_digest']}`",
        f"- samples per language: {identity['repeat']}",
        "- runtime network: disabled",
        "",
        "| Language | Sample | Result |",
        "|---|---:|---|",
    ]
    for item in campaign.get("samples", []):
        lines.append(f"| {item['language']} | {item['sample']} | [{item['path']}/result.json]({item['path']}/result.json) |")
    return "\n".join(lines) + "\n"


def run_embedded_campaign(
    layout: dict[str, Path],
    *,
    jobs: int,
    repeat: int,
    rebuild_images: bool,
    resume: bool,
    language: str | None,
) -> Path:
    zccache_sha = perf_standalone._ensure_clean_checkout()
    soldr_sha = perf_local.git_head(layout["soldr_src"])
    soldr_binary = layout["bin_soldr"] / "soldr"
    if not soldr_binary.is_file():
        raise FileNotFoundError(f"soldr binary missing at {soldr_binary}")
    build_command = build_images(rebuild=rebuild_images)
    subprocess.run(
        ["docker", "volume", "create", SOLDR_HOME_VOLUME],
        capture_output=True,
        text=True,
        check=True,
    )
    host = perf_standalone._host_identity()
    selected = (language,) if language else LANGUAGES
    identity = {
        "zccache_sha": zccache_sha,
        "soldr_sha": soldr_sha,
        "image_digest": _image_digest(),
        "host_fingerprint": host["fingerprint"],
        "fixture_sha256": fixture_sha256(),
        "repeat": repeat,
        "languages": list(selected),
    }
    campaign_dir = default_campaign_dir(zccache_sha, soldr_sha, repeat, language)
    campaign_dir.mkdir(parents=True, exist_ok=True)
    identity_path = campaign_dir / "campaign-identity.json"
    if identity_path.exists():
        if not resume:
            raise RuntimeError(f"campaign already exists: {campaign_dir}; pass --resume")
        validate_resume_identity(json.loads(identity_path.read_text(encoding="utf-8")), identity)
    else:
        write_json(identity_path, identity)

    campaign_path = campaign_dir / "campaign.json"
    if resume and campaign_path.exists():
        campaign = json.loads(campaign_path.read_text(encoding="utf-8"))
        validate_resume_identity(campaign.get("identity", {}), identity)
        validate_completed_samples(campaign_dir, campaign)
    else:
        campaign = {
            "schema_version": 1,
            "identity": identity,
            "host": host,
            "commands": {"image_build": build_command},
            "samples": [],
        }
    completed = {(item["language"], int(item["sample"])) for item in campaign.get("samples", [])}
    for selected_language in selected:
        samples: list[tuple[Path, dict[str, Any]]] = []
        for sample_index in range(1, repeat + 1):
            sample_dir = campaign_dir / selected_language / f"sample-{sample_index:02d}"
            if (selected_language, sample_index) in completed:
                samples.append((sample_dir, _load_sample(sample_dir)))
                continue
            perf_standalone._require_quiet_host()
            if sample_dir.exists():
                shutil.rmtree(sample_dir)
            sample_dir.mkdir(parents=True, exist_ok=False)
            command = _docker_command(
                layout=layout,
                results_dir=sample_dir,
                language=selected_language,
                identity=identity,
                jobs=jobs,
                sample_index=sample_index,
            )
            started = time.monotonic()
            completed_run = subprocess.run(command, check=False)
            if completed_run.returncode != 0:
                raise RuntimeError(f"embedded {selected_language} sample {sample_index} failed with status {completed_run.returncode}")
            result = _load_sample(sample_dir)
            samples.append((sample_dir, result))
            campaign["samples"].append(
                {
                    "language": selected_language,
                    "sample": sample_index,
                    "path": sample_dir.relative_to(campaign_dir).as_posix(),
                    "elapsed_seconds": round(time.monotonic() - started, 3),
                    "docker_command": command,
                }
            )
            write_json(campaign_path, campaign)
            (campaign_dir / "README.md").write_text(_render_markdown(campaign), encoding="utf-8")
        write_json(
            campaign_dir / selected_language / "repeat-summary.json",
            build_repeat_summary(selected_language, campaign_dir, samples),
        )
    return campaign_dir
