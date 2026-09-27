"""Guard the Actions-cache footprint of setup-soldr steps (zccache#1677).

Fails when:

* two cache-enabled setup-soldr steps that can run on the same OS with the
  same cook shape (toolchain, prebuild-deps, prebuild-deps-flags,
  cross-targets, dylint toolchain) use different ``cache-key-suffix`` values,
  unless every differing suffix is listed in ``JUSTIFIED_SUFFIXES``;
* the workflows pin more than one setup-soldr ref or resolve more than one
  soldr ``version``;
* a cache-enabled setup-soldr step in a workflow reachable from
  ``pull_request`` can save durable caches.  On a ref listed in
  ``SAVE_CACHE_REFS`` (setup-soldr v0.9.78+, zackees/setup-soldr#527)
  ``save-cache`` unset or ``auto`` means "no saves on pull_request".  On any
  other ref only ``save-cache: false`` or an expression that is false on
  ``pull_request`` counts.  ``save-cache: true`` must be justified in
  ``JUSTIFIED_PR_SAVES`` (keyed ``workflow.yml:job``);
* this repository's first-party action (``uses: ./``) cannot save durable
  caches from pull-request workflows; allow only explicit no-save or a
  main-ref-only save expression;
* a cache-enabled setup-soldr step does not set ``cook-delta: false`` on a
  ref listed in ``COOK_DELTA_REFS``, unless the job is listed in
  ``JUSTIFIED_COOK_DELTA``.  The cook-delta layer saves one
  ``cook-delta-v2-*`` generation per commit and nothing prunes it
  (zackees/setup-soldr#528); cook bases stay on.

The repository-wide cache total is checked separately, online, by
``ci/check_cache_budget.py``.

Usage: ``uv run --no-project --with pyyaml python ci/check_cache_footprint.py``
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import sys

import yaml

ROOT = Path(__file__).resolve().parent.parent

ACTION = "zackees/setup-soldr"

# Suffixes allowed to differ from the rest of their OS x cook shape, with why.
JUSTIFIED_SUFFIXES: dict[str, str] = {
    "dylint": "materializes the nightly dylint toolchain, driver and tools "
    "(dylint-cache) and disables the cargo-registry cache",
    "check-<label>": "cross-target check: its closure is built for "
    "`cross-targets`, not the host",
}

# setup-soldr refs whose main action honors `save-cache: auto|true|false`
# with `auto` (the default) skipping durable saves on pull_request
# (zackees/setup-soldr#527).  Add the new ref on each pin bump.
SAVE_CACHE_REFS: dict[str, str] = {
    "4df8db93438594f50505574d9dc8117505d33362": "setup-soldr v0.9.80",
    "dfbe9627f6cb0226716b61625b99a58949162720": "setup-soldr dfbe962 (#532)",
    "fabebf4ac3867b0008576797d566db0cb18d43c3": "setup-soldr v0.9.78",
}

# setup-soldr refs whose main action honors `cook-delta: true|false`
# (zackees/setup-soldr#528).  Add the new ref on each pin bump.
COOK_DELTA_REFS: dict[str, str] = {
    "4df8db93438594f50505574d9dc8117505d33362": "setup-soldr v0.9.80",
}

# `workflow.yml:job` (or `actions/<name>:steps`) -> why that step may keep
# the per-commit cook-delta layer.  None today.
JUSTIFIED_COOK_DELTA: dict[str, str] = {}

# `workflow.yml:job` -> why that job must save on pull_request (e.g. it seeds
# a cache a later job in the same run restores).  None today.
JUSTIFIED_PR_SAVES: dict[str, str] = {}

# #1677 budget policy: retire the standalone Rust toolchain archive globally,
# and avoid low-reuse build archives for the two macOS lanes whose exact
# production probes showed zero compile-cache hits. Keep the other platforms'
# build-cache profiles unchanged.
MACOS_BUILD_CACHE_JOBS = {
    "ci-check.yml:check": ("inputs.os", "macOS Check"),
    "ci-check.yml:test": ("inputs.os", "macOS Test"),
    "wrapper-e2e.yml:wrapper-e2e": ("matrix.os", "macOS wrapper-e2e"),
    "fs-matrix.yml:matrix": ("matrix.os", "macOS filesystem matrix"),
}

SHAPE_INPUTS = (
    "toolchain",
    "prebuild-deps",
    "prebuild-deps-flags",
    "cross-targets",
    "dylint-toolchain",
)
OS_EXPR = re.compile(r"\$\{\{\s*(inputs|matrix)\.(os|label)\s*\}\}")
ANY_OS = "*"
COOK_OFF_PROBE_SAVE = (
    "${{ github.event_name == 'pull_request' && "
    "github.head_ref == 'probe/cook-off-matrix' && 'false' || 'auto' }}"
)
WINDOWS_COOK_OFF_PROBE_SAVE = (
    "${{ github.event_name == 'pull_request' && "
    "github.head_ref == 'probe/cook-off-matrix' && "
    "(inputs.os == 'windows-latest' || inputs.os == 'windows-11-arm') && "
    "'false' || 'auto' }}"
)
LINUX_WRAPPER_BUILD_CACHE_PROBE = (
    "${{ github.event_name == 'pull_request' && "
    "github.head_ref == 'probe/cook-off-matrix' && "
    "matrix.os == 'ubuntu-latest' && 'false' || "
    "matrix.os != 'macos-15' }}"
)
WINDOWS_TEST_BUILD_CACHE_PROBE = (
    "${{ github.event_name == 'pull_request' && "
    "github.head_ref == 'probe/cook-off-matrix' && "
    "(inputs.os == 'windows-latest' || inputs.os == 'windows-11-arm') && "
    "'false' || inputs.os != 'macos-15' }}"
)
COOK_OFF_PROBE_INPUTS = {
    "${{ github.event_name == 'pull_request' && "
    "github.head_ref == 'probe/cook-off-matrix' && 'none' || 'soldr-cook' }}",
    "${{ github.event_name == 'pull_request' && "
    "github.head_ref == 'probe/cook-off-matrix' && "
    "(inputs.os == 'windows-latest' || inputs.os == 'windows-11-arm') && "
    "'none' || 'soldr-cook' }}",
}


@dataclass(frozen=True)
class Step:
    where: str
    ref: str
    inputs: dict
    oses: frozenset[str]
    pr_reachable: bool
    main_action: bool = True


def _triggers(doc: dict) -> set[str]:
    on = doc.get(True, doc.get("on"))
    if isinstance(on, str):
        return {on}
    if isinstance(on, list):
        return set(on)
    return set(on or {})


def _oses(job: dict) -> frozenset[str]:
    runs_on = str(job.get("runs-on", ""))
    match = re.fullmatch(r"\$\{\{\s*matrix\.(\w+)\s*\}\}", runs_on)
    if match:
        matrix = (job.get("strategy") or {}).get("matrix") or {}
        values = list(matrix.get(match.group(1)) or [])
        values += [
            i[match.group(1)]
            for i in matrix.get("include") or []
            if match.group(1) in i
        ]
        return frozenset(str(v) for v in values) or frozenset({ANY_OS})
    if "${{" in runs_on:
        return frozenset({ANY_OS})
    return frozenset({runs_on})


def _step(where: str, step: dict, oses: frozenset[str], pr: bool) -> Step | None:
    uses = str(step.get("uses", ""))
    if not uses.startswith(ACTION):
        return None
    name, _, ref = uses.partition("@")
    ref = ref.split("#", 1)[0].strip()
    if name != ACTION:  # sub-actions share the pin check only
        return Step(where, ref, {"cache": False}, frozenset(), False, False)
    return Step(where, ref, dict(step.get("with") or {}), oses, pr)


def collect(root: Path) -> list[Step]:
    steps: list[Step] = []
    pr_texts: list[str] = []
    for path in sorted((root / ".github" / "workflows").glob("*.y*ml")):
        text = path.read_text(encoding="utf-8")
        doc = yaml.safe_load(text) or {}
        triggers = _triggers(doc)
        # A reusable workflow inherits its callers' events; assume a PR caller.
        pr = bool(triggers & {"pull_request", "pull_request_target", "workflow_call"})
        if pr:
            pr_texts.append(text)
        for job_name, job in (doc.get("jobs") or {}).items():
            for index, raw in enumerate(job.get("steps") or []):
                found = _step(f"{path.name}:{job_name}#{index}", raw, _oses(job), pr)
                if found:
                    steps.append(found)
    # Composite actions: PR-reachable when a PR-reachable workflow uses them.
    for path in sorted((root / ".github" / "actions").glob("*/action.y*ml")):
        doc = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        local = f"./.github/actions/{path.parent.name}"
        pr = any(local in text for text in pr_texts)
        where = f"actions/{path.parent.name}"
        for index, raw in enumerate((doc.get("runs") or {}).get("steps") or []):
            found = _step(f"{where}:steps#{index}", raw, frozenset({ANY_OS}), pr)
            if found:
                steps.append(found)
    return steps


def _cache_on(step: Step) -> bool:
    return str(step.inputs.get("cache", True)).lower() not in {"false", "0"}


def _normal_suffix(step: Step) -> str:
    return OS_EXPR.sub(
        lambda m: f"<{m.group(2)}>",
        str(step.inputs.get("cache-key-suffix", "")).strip(),
    )


def _shape(step: Step) -> tuple[str, ...]:
    return tuple(
        "soldr-cook"
        if k == "prebuild-deps" and _is_cook_off_probe_input(step.inputs.get(k, ""))
        else str(step.inputs.get(k, "<default>")).strip()
        for k in SHAPE_INPUTS
    )


def _normal_expression(value: object) -> str:
    return re.sub(r"\s+", "", str(value).strip()).replace('"', "'")


def _is_cook_off_probe_input(value: object) -> bool:
    normalized = _normal_expression(value)
    return normalized in {_normal_expression(item) for item in COOK_OFF_PROBE_INPUTS}


def _cannot_save_on_pr(step: Step) -> bool:
    value = str(step.inputs.get("save-cache", "")).strip().replace(" ", "")
    if step.ref in SAVE_CACHE_REFS and value.lower() in {"", "auto"}:
        return True
    return value.lower() == "false" or value in {
        "${{github.event_name!='pull_request'}}",
        "${{github.event_name=='push'}}",
        "${{github.ref=='refs/heads/main'}}",
        _normal_expression(COOK_OFF_PROBE_SAVE),
        _normal_expression(WINDOWS_COOK_OFF_PROBE_SAVE),
    }


def _local_action_errors(root: Path) -> list[str]:
    errors: list[str] = []
    for path in sorted((root / ".github" / "workflows").glob("*.y*ml")):
        doc = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        pr = bool(
            _triggers(doc) & {"pull_request", "pull_request_target", "workflow_call"}
        )
        if not pr:
            continue
        for job_name, job in (doc.get("jobs") or {}).items():
            for index, raw in enumerate(job.get("steps") or []):
                if str(raw.get("uses", "")).strip() != "./":
                    continue
                step = Step(
                    f"{path.name}:{job_name}#{index}",
                    "./",
                    dict(raw.get("with") or {}),
                    frozenset(),
                    True,
                )
                if not _cannot_save_on_pr(step):
                    errors.append(
                        f"{step.where} local action can save caches on pull_request; "
                        "set save-cache: false or gate saves to refs/heads/main"
                    )
    return errors


def check(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    errors.extend(_local_action_errors(root))
    steps = collect(root)

    refs = sorted({s.ref for s in steps})
    if len(refs) > 1:
        errors.append(f"setup-soldr is pinned at {len(refs)} refs: {', '.join(refs)}")
    versions = sorted(
        {str(s.inputs.get("version", "latest")) for s in steps if s.main_action}
    )
    if len(versions) > 1:
        errors.append(
            f"workflows resolve {len(versions)} soldr versions: {', '.join(versions)}"
        )

    cached = [s for s in steps if _cache_on(s)]

    for step in steps:
        if not step.main_action:
            continue
        solo_toolchain_cache = str(
            step.inputs.get("solo-toolchain-cache", "")
        ).strip().lower()
        if solo_toolchain_cache != "false":
            errors.append(
                f"{step.where} must set solo-toolchain-cache: false; "
                "the standalone toolchain archives are retired by #1677"
            )

    for target, (os_context, label) in MACOS_BUILD_CACHE_JOBS.items():
        target_steps = [
            step
            for step in steps
            if step.main_action and step.where.startswith(f"{target}#")
            and (
                target != "wrapper-e2e.yml:wrapper-e2e"
                or step.inputs.get("prebuild-deps") == "soldr-cook"
                or _is_cook_off_probe_input(step.inputs.get("prebuild-deps", ""))
            )
        ]
        if not target_steps:
            if root.resolve() == ROOT:
                errors.append(
                    f"{target} is missing the guarded {label} cache producer"
                )
            continue
        expected = _normal_expression(
            f"${{{{ {os_context} != 'macos-15' }}}}"
        )
        for step in target_steps:
            actual = _normal_expression(step.inputs.get("build-cache", "true"))
            if (
                target == "wrapper-e2e.yml:wrapper-e2e"
                and actual == _normal_expression(LINUX_WRAPPER_BUILD_CACHE_PROBE)
            ):
                actual = expected
            if (
                target == "ci-check.yml:test"
                and actual == _normal_expression(WINDOWS_TEST_BUILD_CACHE_PROBE)
            ):
                actual = expected
            if actual != expected:
                errors.append(
                    f"{step.where} must disable build-cache on macos-15 "
                    f"({label}; expected ${{{{ {os_context} != 'macos-15' }}}})"
                )

    for i, a in enumerate(cached):
        for b in cached[i + 1 :]:
            if _shape(a) != _shape(b):
                continue
            if not (ANY_OS in a.oses or ANY_OS in b.oses or a.oses & b.oses):
                continue
            sa, sb = _normal_suffix(a), _normal_suffix(b)
            if sa == sb:
                continue
            if all(s in JUSTIFIED_SUFFIXES for s in (sa, sb) if s):
                continue
            errors.append(
                f"{a.where} (cache-key-suffix {sa!r}) and {b.where} ({sb!r}) cook the same "
                "OS x feature shape; share one suffix or justify it in JUSTIFIED_SUFFIXES"
            )

    for s in cached:
        if not s.pr_reachable or _cannot_save_on_pr(s):
            continue
        job = s.where.rsplit("#", 1)[0]
        if str(s.inputs.get("save-cache", "")).strip().lower() == "true":
            if job not in JUSTIFIED_PR_SAVES:
                errors.append(
                    f"{s.where} sets save-cache: true on a pull_request-reachable "
                    "workflow; justify it in JUSTIFIED_PR_SAVES"
                )
            continue
        errors.append(
            f"{s.where} can save caches on pull_request; pin setup-soldr "
            "v0.9.78+ (SAVE_CACHE_REFS) with save-cache: auto"
        )
    for s in cached:
        if not s.main_action:
            continue
        job = s.where.rsplit("#", 1)[0]
        if job in JUSTIFIED_COOK_DELTA:
            continue
        value = str(s.inputs.get("cook-delta", "")).strip().lower()
        if value != "false":
            errors.append(
                f"{s.where} does not set cook-delta: false; the per-commit "
                "cook-delta layer is disabled (setup-soldr#528) unless justified "
                "in JUSTIFIED_COOK_DELTA"
            )
        elif s.ref not in COOK_DELTA_REFS:
            errors.append(
                f"{s.where} sets cook-delta: false on setup-soldr {s.ref}, which "
                "does not honor it; pin a ref listed in COOK_DELTA_REFS"
            )
    return errors


def main() -> int:
    errors = check()
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if not errors:
        print("cache footprint: ok")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
