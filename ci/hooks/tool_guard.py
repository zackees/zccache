#!/usr/bin/env python3
"""PreToolUse hook: blocks bare Rust commands and bare python/pip.

All cargo/rustc/rustfmt must go through the project-root trampolines
(_cargo, _rustc, _rustfmt) which prepend .cargo/bin to PATH ensuring
the rustup toolchain is always used.

All python must go through uv (ensures correct environment).

Exit codes:
  0 - Allow (outputs JSON hookSpecificOutput to deny if needed)
"""

import json
import re
import sys


RUST_TOOLS = {"cargo", "rustc", "rustfmt", "clippy-driver", "cargo-clippy", "cargo-fmt"}
PYTHON_TOOLS = {"python", "python3", "pip", "pip3"}

# Trampoline scripts at project root that normalize the toolchain PATH
RUST_TRAMPOLINES = {"_cargo", "_rustc", "_rustfmt"}

ALLOWED_PREFIXES = ("uv run ", "uv pip ", "soldr ")


FORBIDDEN_SCRIPT_DIRS = re.compile(
    r"""(?:^|[\s/\\])      # start or separator
        (?:bench|tests?)   # forbidden directories
        [/\\]              # path separator
        \S*\.py            # any .py file
    """,
    re.VERBOSE,
)

DENY_PYTHON_IN_CODE = (
    "Do not use Python for benchmarks or tests. "
    "Write them in Rust instead. Python is only for CI scripts and packaging."
)


def _first_word(seg):
    """Extract the first word (command name) from a segment."""
    words = seg.split()
    return words[0] if words else ""


def _resolve_uv_run_tool(seg):
    """If seg starts with 'uv run', return the tool being invoked.

    Handles both 'uv run cargo ...' and 'uv run --script ./_cargo ...' forms.
    """
    # 'uv run --script ./_cargo ...' — trampoline invocation, return the script name
    m = re.match(r"uv\s+run\s+--script\s+(\S+)", seg)
    if m:
        return m.group(1)
    # 'uv run cargo ...' — bare tool
    m = re.match(r"uv\s+run\s+(\S+)", seg)
    return m.group(1) if m else None


def check_command(command):
    """Check a command string for forbidden bare invocations.

    Returns (tool, reason) if forbidden, None if allowed.
    """
    # ── Global check: block .py scripts in bench/ or tests/ dirs ─────
    if FORBIDDEN_SCRIPT_DIRS.search(command):
        return ("python", DENY_PYTHON_IN_CODE)

    # ── Per-segment checks ───────────────────────────────────────────
    segments = re.split(r"&&|\|\||;", command)

    for seg in segments:
        seg = seg.strip()
        if not seg:
            continue

        first = _first_word(seg)

        # Allow trampoline scripts (with or without ./ prefix)
        bare = first.lstrip("./")
        if bare in RUST_TRAMPOLINES:
            continue

        # Block `uv run cargo/rustc/...` — must use _cargo/_rustc trampolines
        if seg.startswith("uv run ") or seg.startswith("uv  run "):
            tool = _resolve_uv_run_tool(seg)
            if tool is None:
                continue
            # Allow trampoline invocations: uv run --script ./_cargo
            tool_bare = tool.lstrip("./")
            if tool_bare in RUST_TRAMPOLINES:
                continue
            # Block direct Rust tool invocations via uv run
            if tool in RUST_TOOLS:
                trampoline = f"_{tool}" if f"_{tool}" in RUST_TRAMPOLINES else "_cargo"
                return (
                    tool,
                    f"Use `./{trampoline} ...` or `soldr {tool} ...` instead "
                    f"of `uv run {tool} ...`. Both resolve the rustup-managed "
                    f"toolchain; `uv run <rust-tool>` bypasses that.",
                )
            # Other uv run commands are fine (uv run python, uv run --script, etc.)
            continue

        # Allow uv pip
        if seg.startswith("uv pip "):
            continue

        # Allow `soldr ...` — soldr resolves the rustup-managed toolchain
        # the same way the _cargo / _rustc / _rustfmt trampolines do.
        if seg.startswith("soldr "):
            continue

        # Block bare Rust tools
        if first in RUST_TOOLS:
            trampoline = f"_{first}" if f"_{first}" in RUST_TRAMPOLINES else "_cargo"
            return (
                first,
                f"Use `./{trampoline} ...` instead of bare `{first}`. "
                f"Trampolines ensure the correct rustup toolchain is on PATH.",
            )

        # Block bare Python tools
        if first in PYTHON_TOOLS:
            if first.startswith("pip"):
                suggestion = f"uv pip {' '.join(seg.split()[1:])}" if len(seg.split()) > 1 else "uv pip ..."
                return (
                    first,
                    f"Use `{suggestion}` instead of bare `{first}`. "
                    f"All pip operations must go through uv.",
                )
            return (
                first,
                f"Use `uv run ...` instead of bare `{first}`. "
                f"All Python must be executed through uv.",
            )

    return None


def deny(reason):
    """Output a JSON deny response."""
    json.dump({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }, sys.stdout)


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)

    # Only check Bash commands
    tool_name = data.get("tool_name", "")
    if tool_name != "Bash":
        sys.exit(0)

    command = data.get("tool_input", {}).get("command", "")
    if not command:
        sys.exit(0)

    result = check_command(command)
    if result:
        _, reason = result
        deny(reason)

    sys.exit(0)


if __name__ == "__main__":
    main()
