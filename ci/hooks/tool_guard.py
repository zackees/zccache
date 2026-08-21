#!/usr/bin/env python3
"""PreToolUse hook: blocks bare Rust commands and bare python/pip."""

import json
import re
import sys


RUST_TOOLS = {
    "cargo",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "cargo-clippy",
    "cargo-fmt",
    # `rustup` is also gated — `rustup run <toolchain> cargo ...` and
    # `rustup which ...` are the usual escape hatches around soldr's
    # toolchain selection. `soldr rustup` is a documented passthrough.
    "rustup",
    "rustdoc",
    "rust-gdb",
    "rust-lldb",
    "rust-analyzer",
}
PYTHON_TOOLS = {"python", "python3", "pip", "pip3"}
LEGACY_RUST_TRAMPOLINES = {"_cargo", "_rustc", "_rustfmt"}
SHELL_WRAPPERS = {"cmd", "powershell", "pwsh", "bash", "sh", "zsh"}
# `uv run` option arity, transcribed from `uv help run`.
#
# Arity is load-bearing, not cosmetic. `_resolve_uv_run_tool` skips two words
# for a value-taking option and one for a boolean; misfiling a boolean as
# value-taking makes it swallow the very word it was supposed to inspect, so
# `uv run --frozen cargo build` resolves its "tool" to `build` and sails past
# the soldr gate. Misfiling the other way is milder but still wrong: the
# option's value is mistaken for the tool.
#
# Both sets are asserted against the guard's own behaviour by
# `test_no_uv_run_option_prefix_hides_a_rust_tool`, so a wrong entry here
# fails the suite rather than silently opening a hole.

# Options that consume the following word.
UV_RUN_OPTIONS_WITH_VALUE = {
    "--allow-insecure-host",
    "--cache-dir",
    "--color",
    "--config-file",
    "--config-setting",
    "--config-settings-package",
    "--default-index",
    "--directory",
    "--env-file",
    "--exclude-newer",
    "--exclude-newer-package",
    "--extra",
    "--extra-index-url",
    "--find-links",
    "--fork-strategy",
    "--group",
    "--index",
    "--index-strategy",
    "--index-url",
    "--keyring-provider",
    "--link-mode",
    "--no-binary-package",
    "--no-build-isolation-package",
    "--no-build-package",
    "--no-editable-package",
    "--no-extra",
    "--no-group",
    "--no-sources-package",
    "--only-group",
    "--package",
    "--prerelease",
    "--prerelease-package",
    "--project",
    "--python",
    "--python-platform",
    "--refresh-package",
    "--reinstall-package",
    "--resolution",
    "--upgrade-group",
    "--upgrade-package",
    "--with",
    "--with-editable",
    "--with-requirements",
    "-C",
    "-P",
    "-f",
    "-i",
    "-p",
    "-w",
}

# Options that stand alone. Listed rather than inferred so the
# arity test can prove neither kind hides the tool. `--module`/`-m` and
# `--script`/`-s` belong here: the word after them is the module or script
# being run, i.e. exactly the token the guard must look at.
UV_RUN_BOOLEAN_OPTIONS = {
    "--active",
    "--all-extras",
    "--all-groups",
    "--all-packages",
    "--compile-bytecode",
    "--exact",
    "--frozen",
    "--gui-script",
    "--help",
    "--isolated",
    "--locked",
    "--managed-python",
    "--module",
    "--no-binary",
    "--no-build",
    "--no-build-isolation",
    "--no-cache",
    "--no-config",
    "--no-default-groups",
    "--no-dev",
    "--no-editable",
    "--no-env-file",
    "--no-index",
    "--no-managed-python",
    "--no-progress",
    "--no-project",
    "--no-python-downloads",
    "--no-sources",
    "--no-sync",
    "--offline",
    "--only-dev",
    "--quiet",
    "--refresh",
    "--reinstall",
    "--script",
    "--system-certs",
    "--upgrade",
    "--verbose",
    "-U",
    "-h",
    "-m",
    "-n",
    "-q",
    "-s",
    "-v",
}

# Path-shape: any path component named `bench`, `test`, or `tests` followed
# by a `.py` file. Used as a per-argument check on segments we've already
# identified as Python invocations — NOT a top-level command-string match
# (that's what made the old check too broad).
#
# Anchored to `(^|sep)` so `tests-fixture/foo.py` (no path separator
# directly before `tests`) doesn't false-positive. Backslash + forward-slash
# both count as separators so Windows paths match identically.
FORBIDDEN_PATH_RE = re.compile(
    r"""(?:^|[/\\])        # start or path separator
        (?:bench|tests?)   # forbidden directory name
        [/\\]              # path separator
        \S*\.py            # any .py file
    """,
    re.VERBOSE,
)

DENY_PYTHON_IN_CODE = (
    "Do not use Python for benchmarks or tests. "
    "Write them in Rust instead. Python is only for CI scripts and packaging."
)

SHELL_TOOL_NAMES = {
    "Bash",
    "Shell",
    "PowerShell",
    "shell_command",
    "functions.shell_command",
}


def extract_command(data):
    """Return shell text from Codex/Claude tool payload variants."""
    tool_input = data.get("tool_input") or data.get("toolInput") or {}
    if isinstance(tool_input, str):
        return tool_input
    if not isinstance(tool_input, dict):
        return ""

    for key in ("command", "script"):
        value = tool_input.get(key)
        if isinstance(value, str):
            return value

    argv = tool_input.get("argv")
    if isinstance(argv, list):
        return " ".join(str(part) for part in argv)

    return ""


def _args_contain_forbidden_test_path(words):
    """True if any argument matches a `bench/` or `tests/` Python file path."""
    return any(FORBIDDEN_PATH_RE.search(w) for w in words)


def _is_env_assignment(word):
    return re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", word) is not None


def _split_shell_segments(command):
    segments = []
    buf = []
    quote = None
    i = 0
    while i < len(command):
        ch = command[i]
        if quote is not None:
            buf.append(ch)
            if ch == quote:
                quote = None
            i += 1
            continue

        if ch in {"'", '"'}:
            quote = ch
            buf.append(ch)
            i += 1
            continue

        is_double_amp = ch == "&" and i + 1 < len(command) and command[i + 1] == "&"
        is_double_pipe = ch == "|" and i + 1 < len(command) and command[i + 1] == "|"
        if ch in {";", "|", "\r", "\n"} or is_double_amp:
            segment = "".join(buf).strip()
            if segment:
                segments.append(segment)
            buf = []
            i += 2 if is_double_amp or is_double_pipe else 1
            continue

        buf.append(ch)
        i += 1

    segment = "".join(buf).strip()
    if segment:
        segments.append(segment)
    return segments


def _tokenize(segment):
    words = []
    buf = []
    quote = None
    for ch in segment:
        if quote is not None:
            if ch == quote:
                quote = None
            else:
                buf.append(ch)
            continue

        if ch in {"'", '"'}:
            quote = ch
            continue
        if ch.isspace():
            if buf:
                words.append("".join(buf))
                buf = []
            continue
        buf.append(ch)

    if buf:
        words.append("".join(buf))
    return words


def _program_name(word):
    cleaned = word.strip().strip("'\"").replace("\\", "/")
    while cleaned.startswith("./"):
        cleaned = cleaned[2:]
    base = cleaned.rsplit("/", 1)[-1].lower()
    for suffix in (".exe", ".cmd", ".bat", ".ps1"):
        if base.endswith(suffix):
            base = base[: -len(suffix)]
            break
    return base


def _command_words(seg):
    words = _tokenize(seg)
    while words and words[0] in {"&", "call", "exec", "command"}:
        words = words[1:]
    if words and _program_name(words[0]) == "env":
        words = words[1:]
    while words and _is_env_assignment(words[0]):
        words = words[1:]
    return words


def _resolve_uv_run_tool(words):
    if len(words) < 3 or _program_name(words[0]) != "uv" or words[1] != "run":
        return None

    i = 2
    while i < len(words):
        word = words[i]
        if word == "--":
            i += 1
            break
        if word == "--script" and i + 1 < len(words):
            return words[i + 1]
        if word.startswith("--script="):
            return word.split("=", 1)[1]
        if not word.startswith("-"):
            break
        if "=" not in word and word in UV_RUN_OPTIONS_WITH_VALUE:
            i += 2
        else:
            i += 1

    return words[i] if i < len(words) else None


def _nested_shell_command(words):
    if not words:
        return None
    first = _program_name(words[0])
    if first not in SHELL_WRAPPERS:
        return None

    if first == "cmd":
        for i, word in enumerate(words[1:], start=1):
            if word.lower() in {"/c", "/r"} and i + 1 < len(words):
                return " ".join(words[i + 1 :])
        return None

    if first in {"powershell", "pwsh"}:
        for i, word in enumerate(words[1:], start=1):
            if word.lower() in {"-command", "-c", "/c"} and i + 1 < len(words):
                return " ".join(words[i + 1 :])
        return None

    for i, word in enumerate(words[1:], start=1):
        option = word.lower().lstrip("-")
        if "c" in option and i + 1 < len(words):
            return " ".join(words[i + 1 :])
    return None


def check_command(command):
    """Return (tool, reason) if forbidden, otherwise None.

    The forbidden-test-path check (Python-on-bench/tests-*.py) runs per
    segment, after env-var stripping, and ONLY when the segment is a
    Python invocation. This is the deliberate narrowing from a prior
    implementation that matched the test-path regex against the entire
    command string — that approach false-positive-blocked `gh api .../
    tests/foo.py`, `wc -l tests/foo.py`, `uv run pytest tests/foo.py`,
    and any other command that happened to reference a test path as an
    argument. Now only `python tests/foo.py` (and `uv run python
    tests/foo.py`) trigger the block.
    """
    segments = _split_shell_segments(command)

    for seg in segments:
        seg = seg.strip()
        if not seg:
            continue

        words = _command_words(seg)
        if not words:
            continue

        first = _program_name(words[0])
        nested = _nested_shell_command(words)
        if nested is not None:
            result = check_command(nested)
            if result:
                return result
            continue

        if first in LEGACY_RUST_TRAMPOLINES:
            return (
                first,
                f"Use `soldr {first[1:]} ...` instead of legacy `{words[0]}`. "
                "The root Rust trampolines have been removed.",
            )

        if first == "uv" and len(words) > 1 and words[1] == "run":
            tool = _resolve_uv_run_tool(words)
            if tool is None:
                continue
            tool_bare = _program_name(tool)
            if tool_bare in LEGACY_RUST_TRAMPOLINES:
                return (
                    tool_bare,
                    f"Use `soldr {tool_bare[1:]} ...` instead of legacy `{tool}`. "
                    "The root Rust trampolines have been removed.",
                )
            if tool_bare in RUST_TOOLS:
                return (
                    tool_bare,
                    f"Use `soldr {tool_bare} ...` instead of `uv run {tool} ...`. "
                    "`uv run <rust-tool>` bypasses soldr's toolchain selection.",
                )
            # `uv run python tests/foo.py` — Python is the executor and a
            # forbidden path is in the args. Block.
            if tool_bare in PYTHON_TOOLS and not tool_bare.startswith("pip"):
                if _args_contain_forbidden_test_path(words):
                    return ("python", DENY_PYTHON_IN_CODE)
            # `uv run --script tests/foo.py` — `tool` IS the path being
            # executed as a Python script. Block when it's under bench/
            # or tests/.
            if FORBIDDEN_PATH_RE.search(tool):
                return ("python", DENY_PYTHON_IN_CODE)
            continue

        if first == "uv" and len(words) > 1 and words[1] == "pip":
            continue

        if first == "soldr":
            continue

        if first in RUST_TOOLS:
            return (
                first,
                f"Use `soldr {first} ...` instead of bare `{first}`. "
                "soldr resolves the pinned rustup-managed toolchain.",
            )

        if first in PYTHON_TOOLS:
            if first.startswith("pip"):
                suggestion = (
                    f"uv pip {' '.join(seg.split()[1:])}"
                    if len(seg.split()) > 1
                    else "uv pip ..."
                )
                return (
                    first,
                    f"Use `{suggestion}` instead of bare `{first}`. "
                    "All pip operations must go through uv.",
                )
            # `python tests/foo.py` — give the specific test-file message
            # rather than the generic "use uv run". Both block, the
            # specific one tells the author WHY their test file shouldn't
            # be Python.
            if _args_contain_forbidden_test_path(words):
                return ("python", DENY_PYTHON_IN_CODE)
            return (
                first,
                f"Use `uv run ...` instead of bare `{first}`. "
                "All Python must be executed through uv.",
            )

    return None


def deny(reason):
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        },
        sys.stdout,
    )


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)

    if data.get("tool_name", "") not in SHELL_TOOL_NAMES:
        sys.exit(0)

    command = extract_command(data)
    if not command:
        sys.exit(0)

    result = check_command(command)
    if result:
        _, reason = result
        deny(reason)
        print(reason, file=sys.stderr)
        sys.exit(2)

    sys.exit(0)


if __name__ == "__main__":
    main()
