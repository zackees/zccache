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
* a setup-soldr step still writes the bare main-push-only rule
  (``MAIN_PUSH_ONLY_SAVE``) instead of ``SAVE_CACHE_POLICY``.  The only
  exception to the main-push-only rule is nektos/act: local bosn runs are
  ``pull_request`` events with ``ACT=true`` and save fully so they stay warm
  (zackees/bosn#309); on GitHub ``env.ACT`` is empty and the policy reduces
  to main-push-only, which ``check`` re-proves by evaluating it;
* this repository's first-party action (``uses: ./``) cannot save durable
  caches from pull-request workflows; allow only explicit no-save or a
  main-ref-only save expression;
* a cache-enabled setup-soldr step does not set ``cook-delta: false`` on a
  ref listed in ``COOK_DELTA_REFS``, unless the job is listed in
  ``JUSTIFIED_COOK_DELTA``.  The cook-delta layer saves one
  ``cook-delta-v2-*`` generation per commit and nothing prunes it
  (zackees/setup-soldr#528); cook bases stay on;
* a cache-enabled setup-soldr step that can run on a non-Linux runner
  cooks dependencies.  Cook bases run on Linux only (zackees/ci.yml#5,
  RUST-010; zccache#1758): the macOS/Windows generations pushed a full
  lock-transition re-seed past the pre-prune target.  Such a step must set
  ``prebuild-deps: none`` or ``LINUX_ONLY_COOK`` for its OS context.

The repository-wide cache total is checked separately, online, by
``ci/check_cache_budget.py``.

Usage: ``uv run --no-project --with pyyaml python ci/check_cache_footprint.py``
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

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
    "a07bab94f16124b5c6857b137a237a53a61e06d1": "setup-soldr v0.9.82",
    "4df8db93438594f50505574d9dc8117505d33362": "setup-soldr v0.9.80",
    "dfbe9627f6cb0226716b61625b99a58949162720": "setup-soldr dfbe962 (#532)",
    "fabebf4ac3867b0008576797d566db0cb18d43c3": "setup-soldr v0.9.78",
}

# setup-soldr refs whose main action honors `cook-delta: true|false`
# (zackees/setup-soldr#528).  Add the new ref on each pin bump.
COOK_DELTA_REFS: dict[str, str] = {
    "a07bab94f16124b5c6857b137a237a53a61e06d1": "setup-soldr v0.9.82",
    "4df8db93438594f50505574d9dc8117505d33362": "setup-soldr v0.9.80",
}

# `workflow.yml:job` (or `actions/<name>:steps`) -> why that step may keep
# the per-commit cook-delta layer.  None today.
JUSTIFIED_COOK_DELTA: dict[str, str] = {}

# `workflow.yml:job` -> why that job must save on pull_request (e.g. it seeds
# a cache a later job in the same run restores).  None today.
JUSTIFIED_PR_SAVES: dict[str, str] = {}

# #1677 budget rule: on GitHub only a push to main saves durable caches.
MAIN_PUSH_ONLY_SAVE = (
    "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' "
    "&& 'auto' || 'false' }}"
)
# The save-cache value every setup-soldr step that saves on main must use.
# nektos/act sets ACT=true, so local bosn runs (deliberately pull_request
# events) save fully and stay warm (zackees/bosn#309).  On GitHub `env.ACT`
# is empty, so this reduces to MAIN_PUSH_ONLY_SAVE and a pull_request run
# never saves; `check` proves that with `evaluate_save_policy`.  Revisit when
# setup-soldr's runner-aware `auto` (zackees/setup-soldr#537) ships.
SAVE_CACHE_POLICY = (
    "${{ env.ACT && 'true' || (github.event_name == 'push' "
    "&& github.ref == 'refs/heads/main' && 'auto' || 'false') }}"
)
# ci-check.yml's Test job: SAVE_CACHE_POLICY, except its Windows legs never
# save; they restore wrapper-e2e's windows-x64 caches (#1741 budget unchanged).
WINDOWS_TEST_SAVE_CACHE_POLICY = (
    "${{ env.ACT && 'true' || (github.event_name == 'push' "
    "&& github.ref == 'refs/heads/main' && inputs.os != 'windows-latest' "
    "&& inputs.os != 'windows-11-arm' && 'auto' || 'false') }}"
)
# The composite build-target action forwards its caller's GitHub policy and
# adds the act prefix once, so callers keep MAIN_PUSH_ONLY_SAVE or "false".
COMPOSITE_SAVE_CACHE_POLICY = "${{ env.ACT && 'true' || inputs.save_cache }}"

_EXPR_TOKEN = re.compile(r"\s*('[^']*'|&&|\|\||==|!=|\(|\)|[A-Za-z_][\w.]*)")


def evaluate_save_policy(
    expression: str, *, event_name: str, ref: str, act: str = ""
) -> str:
    """Evaluate a ``save-cache`` expression the way Actions would.

    Supports the subset the save policies use: single-quoted strings,
    ``env.ACT``, ``github.event_name``, ``github.ref``, ``==``/``!=``,
    ``&&``/``||`` (value-returning, empty string is falsy) and parentheses.
    ``inputs.*`` (a reusable workflow's inputs) is unknown: it may appear only
    where short-circuiting makes it irrelevant, e.g. after a
    ``github.event_name == 'push' &&`` that is false on ``pull_request``.
    Anything else, or a result that depends on an input, raises
    ``ValueError`` so an unprovable policy fails closed.
    """
    body = expression.strip()
    if not (body.startswith("${{") and body.endswith("}}")):
        raise ValueError(f"not an expression: {expression!r}")
    body = body[3:-2].strip()
    tokens: list[str] = []
    pos = 0
    while pos < len(body):
        match = _EXPR_TOKEN.match(body, pos)
        if not match:
            raise ValueError(f"unsupported syntax at {body[pos:]!r}")
        tokens.append(match.group(1))
        pos = match.end()
        while pos < len(body) and body[pos].isspace():
            pos += 1
    names = {"env.ACT": act, "github.event_name": event_name, "github.ref": ref}

    def primary(i: int) -> tuple[str, int]:
        token = tokens[i] if i < len(tokens) else ""
        if token == "(":
            value, i = either(i + 1)
            if i >= len(tokens) or tokens[i] != ")":
                raise ValueError(f"unbalanced parentheses in {expression!r}")
            return value, i + 1
        if token.startswith("'"):
            return token[1:-1], i + 1
        if token in names:
            return names[token], i + 1
        if token.startswith("inputs."):
            return None, i + 1
        raise ValueError(f"unsupported token {token!r} in {expression!r}")

    def compare(i: int) -> tuple[str, int]:
        left, i = primary(i)
        while i < len(tokens) and tokens[i] in {"==", "!="}:
            op = tokens[i]
            right, i = primary(i + 1)
            if left is None or right is None:
                left = None
                continue
            equal = left.lower() == right.lower()
            left = "true" if equal == (op == "==") else ""
        return left, i

    def both(i: int) -> tuple[str, int]:
        left, i = compare(i)
        while i < len(tokens) and tokens[i] == "&&":
            right, i = compare(i + 1)
            if left is None:
                left = "" if right == "" else None
            elif left:
                left = right
        return left, i

    def either(i: int) -> tuple[str, int]:
        left, i = both(i)
        while i < len(tokens) and tokens[i] == "||":
            right, i = both(i + 1)
            if left is None:
                left = None
            elif not left:
                left = right
        return left, i

    value, end = either(0)
    if end != len(tokens):
        raise ValueError(f"trailing tokens in {expression!r}")
    if value is None:
        raise ValueError(f"result depends on a workflow input: {expression!r}")
    return value


def _save_policy_errors() -> list[str]:
    """Prove SAVE_CACHE_POLICY is main-push-only on GitHub and full under act."""
    cases = (
        ("pull_request", "refs/pull/1/merge", "", "false"),
        ("push", "refs/heads/feature", "", "false"),
        ("schedule", "refs/heads/main", "", "false"),
        ("push", "refs/heads/main", "", "auto"),
        ("pull_request", "refs/pull/1/merge", "true", "true"),
    )
    errors: list[str] = []
    for event_name, ref, act, expected in cases:
        actual = evaluate_save_policy(
            SAVE_CACHE_POLICY, event_name=event_name, ref=ref, act=act
        )
        if actual != expected:
            errors.append(
                f"SAVE_CACHE_POLICY evaluates to {actual!r} for {event_name} "
                f"{ref} (ACT={act!r}); expected {expected!r}"
            )
    return errors

# #1677 budget policy: retire low-return archives identified by exact hosted
# probes. #1758: cook bases are Linux-only (zackees/ci.yml#5 RUST-010); keep
# Windows wrapper build-cache and all registries.
MACOS_BUILD_CACHE_JOBS = {
    "ci-check.yml:check": ("inputs.os", "macOS Check"),
    "fs-matrix.yml:matrix": ("matrix.os", "macOS filesystem matrix"),
}

# Measured steady-state cuts. These are exact workflow call sites so a new or
# defaulted producer cannot silently recreate a retired cache identity.
COOK_OFF_CALLS = {
    "wrapper-e2e.yml:wrapper-e2e#2": "none",
    "wrapper-e2e.yml:wrapper-e2e#3": "none",
    "ci.yml:dylint#3": "none",
}
REGISTRY_RESTORE_CALLS = {
    "wrapper-e2e.yml:wrapper-e2e#2": True,
    "wrapper-e2e.yml:wrapper-e2e#3": True,
}
LINUX_WRAPPER_BUILD_CACHE = "false"
# Windows x64 Test restores wrapper-e2e's windows-x64 build cache but never
# saves (its save-cache excludes Windows), so the #1741 budget is unchanged.
WINDOWS_TEST_BUILD_CACHE = "${{ inputs.os != 'macos-15' && inputs.os != 'windows-11-arm' }}"
# The only accepted cook expression on a step that can run off Linux.
LINUX_ONLY_COOK = "${{ startsWith(%s, 'ubuntu') && 'soldr-cook' || 'none' }}"
# Reusable workflows whose `inputs.os` callers are all Linux, with why.
LINUX_ONLY_REUSABLE = {
    "ci-check-cross.yml": "called only from ci-linux.yml, on ubuntu runners",
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
    return tuple(str(step.inputs.get(k, "<default>")).strip() for k in SHAPE_INPUTS)


def _cannot_save_on_pr(step: Step) -> bool:
    value = str(step.inputs.get("save-cache", "")).strip().replace(" ", "")
    main_push_only = _normal_expression(MAIN_PUSH_ONLY_SAVE)
    main_push_boolean = "${{github.event_name=='push'&&github.ref=='refs/heads/main'}}"
    if step.ref in SAVE_CACHE_REFS and value.lower() in {"", "auto"}:
        return True
    # SAVE_CACHE_POLICY is main-push-only on GitHub (env.ACT is empty there);
    # _save_policy_errors re-proves that on every check.
    if value == _normal_expression(SAVE_CACHE_POLICY):
        return True
    if value.lower() == "false" or value in {main_push_only, main_push_boolean} or value in {
        "${{github.event_name!='pull_request'}}",
        "${{github.event_name=='push'}}",
        "${{github.ref=='refs/heads/main'}}",
    }:
        return True
    # Any other expression must evaluate to a non-saving value on a GitHub
    # pull_request, whatever its workflow inputs are.
    try:
        on_pr = evaluate_save_policy(
            str(step.inputs.get("save-cache", "")),
            event_name="pull_request",
            ref="refs/pull/1/merge",
        )
    except ValueError:
        return False
    return on_pr.lower() in {"", "false"}


def _normal_expression(value: object) -> str:
    text = str(value).lower() if isinstance(value, bool) else str(value)
    return re.sub(r"\s+", "", text.strip()).replace('"', "'")


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


def _fs_matrix_windows_profile_errors(root: Path) -> list[str]:
    """Enforce the measured Windows FS cache shape and disabled build layer."""
    path = root / ".github/workflows/fs-matrix.yml"
    if not path.exists():
        return []
    workflow = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    steps = (workflow.get("jobs") or {}).get("matrix", {}).get("steps") or []
    setup = [
        step
        for step in steps
        if str(step.get("uses", "")).startswith(f"{ACTION}@")
    ]
    windows = [step for step in setup if step.get("if") == "matrix.os == 'windows-latest'"]
    other = [step for step in setup if step.get("if") == "matrix.os != 'windows-latest'"]
    if len(setup) != 2 or len(windows) != 1 or len(other) != 1:
        return [
            (
                "fs-matrix.yml:matrix must split the measured Windows cache profile "
                "from the other OS setup"
            )
        ]

    errors: list[str] = []
    windows_with = windows[0].get("with") or {}
    expected_save = SAVE_CACHE_POLICY
    expected_windows = {
        "build-cache": False,
        "cargo-registry-cache": True,
        "prebuild-deps": "none",
    }
    for key, expected in expected_windows.items():
        if windows_with.get(key) != expected:
            errors.append(
                f"fs-matrix.yml:matrix Windows setup must keep {key}={expected!r}; "
                "cook bases are Linux-only (#1758) and this lane saves no build-cache"
            )
    if windows_with.get("save-cache") != expected_save:
        errors.append(
            "fs-matrix.yml:matrix Windows setup must retain the main-push-only "
            "save-cache policy (SAVE_CACHE_POLICY)"
        )

    other_with = other[0].get("with") or {}
    expected_other_build = "${{ matrix.os != 'macos-15' }}"
    if _normal_expression(other_with.get("build-cache", "true")) != _normal_expression(
        expected_other_build
    ):
        errors.append(
            "fs-matrix.yml:matrix non-Windows setup must preserve the measured "
            "Linux build-cache and macOS no-build-cache policy"
        )
    return errors


def _non_linux_cook_errors(steps: list[Step]) -> list[str]:
    """#1758: only Linux runners may produce cook-base caches."""
    allowed = {
        _normal_expression(LINUX_ONLY_COOK % context)
        for context in ("inputs.os", "matrix.os")
    }
    errors: list[str] = []
    for step in steps:
        if not step.main_action or not _cache_on(step):
            continue
        # Composite actions take `prebuild_deps` from their callers; the
        # release matrix passes `none` for every non-Linux target.
        if step.where.startswith("actions/"):
            continue
        if step.where.split(":", 1)[0] in LINUX_ONLY_REUSABLE:
            continue
        if not any(os == ANY_OS or not os.startswith("ubuntu") for os in step.oses):
            continue
        value = _normal_expression(step.inputs.get("prebuild-deps", "soldr-cook"))
        if value == "none" or value in allowed:
            continue
        errors.append(
            f"{step.where} can run on a non-Linux runner and cooks dependencies; "
            "cook bases are Linux-only (#1758): set prebuild-deps: none or "
            f"{LINUX_ONLY_COOK % 'matrix.os'!r}"
        )
    return errors


def _act_save_policy_errors(steps: list[Step]) -> list[str]:
    """Main-push-only savers must use SAVE_CACHE_POLICY (act saves fully)."""
    bare = _normal_expression(MAIN_PUSH_ONLY_SAVE)
    inputs_only = _normal_expression("${{ inputs.save_cache }}")
    errors: list[str] = []
    for step in steps:
        if not step.main_action:
            continue
        value = _normal_expression(step.inputs.get("save-cache", ""))
        if value == bare:
            errors.append(
                f"{step.where} uses the bare main-push-only save-cache rule; use "
                "SAVE_CACHE_POLICY so local act runs save fully (zackees/bosn#309)"
            )
        elif step.where.startswith("actions/") and value == inputs_only:
            errors.append(
                f"{step.where} forwards inputs.save_cache without the act prefix; "
                "use COMPOSITE_SAVE_CACHE_POLICY"
            )
    return errors


def check(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    errors.extend(_save_policy_errors())
    errors.extend(_local_action_errors(root))
    errors.extend(_fs_matrix_windows_profile_errors(root))
    steps = collect(root)
    errors.extend(_non_linux_cook_errors(steps))
    errors.extend(_act_save_policy_errors(steps))

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
        solo_toolchain_cache = (
            str(step.inputs.get("solo-toolchain-cache", "")).strip().lower()
        )
        if solo_toolchain_cache != "false":
            errors.append(
                f"{step.where} must set solo-toolchain-cache: false; "
                "the standalone toolchain archives are retired by #1677"
            )

    by_location = {step.where: step for step in steps if step.main_action}
    for where, expected in COOK_OFF_CALLS.items():
        step = by_location.get(where)
        if step is None:
            if root.resolve() == ROOT:
                errors.append(f"{where} is missing its guarded #1677 producer")
            continue
        actual = str(step.inputs.get("prebuild-deps", "<default>")).strip()
        if _normal_expression(actual) != _normal_expression(expected):
            errors.append(f"{where} must use the measured cook policy {expected!r}")

    for where, expected in REGISTRY_RESTORE_CALLS.items():
        step = by_location.get(where)
        if step is None:
            if root.resolve() == ROOT:
                errors.append(f"{where} is missing its guarded registry restore")
            continue
        actual = step.inputs.get("cargo-registry-cache", "<default>")
        if actual is not expected:
            errors.append(
                f"{where} must keep cargo-registry-cache: true for warm parity"
            )

    linux_wrapper = by_location.get("wrapper-e2e.yml:wrapper-e2e#2")
    if linux_wrapper and _normal_expression(
        linux_wrapper.inputs.get("build-cache", "true")
    ) != _normal_expression(LINUX_WRAPPER_BUILD_CACHE):
        errors.append(
            "wrapper-e2e.yml:wrapper-e2e#1 must disable Linux build-cache "
            "while retaining the Windows wrapper cache"
        )
    windows_test = by_location.get("ci-check.yml:test#2")
    if windows_test and (
        _normal_expression(windows_test.inputs.get("build-cache", "true"))
        != _normal_expression(WINDOWS_TEST_BUILD_CACHE)
        or _normal_expression(windows_test.inputs.get("save-cache", ""))
        != _normal_expression(WINDOWS_TEST_SAVE_CACHE_POLICY)
    ):
        errors.append(
            "ci-check.yml:test#1 must restore (never save) the Windows x64 "
            "build-cache, disable ARM64/macOS, and retain Linux Test"
        )

    for target, (os_context, label) in MACOS_BUILD_CACHE_JOBS.items():
        target_steps = [
            step
            for step in steps
            if step.main_action and step.where.startswith(f"{target}#")
        ]
        if not target_steps:
            if root.resolve() == ROOT:
                errors.append(f"{target} is missing the guarded {label} cache producer")
            continue
        expected = _normal_expression(f"${{{{ {os_context} != 'macos-15' }}}}")
        for step in target_steps:
            actual = _normal_expression(step.inputs.get("build-cache", "true"))
            # The guarded Windows-only Filesystem Matrix step is validated by
            # _fs_matrix_windows_profile_errors above.
            if target == "fs-matrix.yml:matrix" and actual == "false":
                continue
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
