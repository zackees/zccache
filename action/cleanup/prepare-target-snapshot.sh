#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

parse_size_bytes() {
  local raw="${1:-0}"
  raw="${raw//[[:space:]]/}"
  local value
  value="$(printf '%s' "$raw" | tr '[:upper:]' '[:lower:]')"

  case "$value" in
    ""|"0"|"false"|"none"|"unlimited")
      printf '0\n'
      return 0
      ;;
  esac

  if [[ ! "$value" =~ ^([0-9]+)([a-z]*)$ ]]; then
    echo "Invalid target snapshot size: $raw" >&2
    return 2
  fi

  local number="${BASH_REMATCH[1]}"
  local unit="${BASH_REMATCH[2]}"
  local multiplier
  case "$unit" in
    ""|"b") multiplier=1 ;;
    "k"|"kb"|"kib") multiplier=1024 ;;
    "m"|"mb"|"mib") multiplier=$((1024 * 1024)) ;;
    "g"|"gb"|"gib") multiplier=$((1024 * 1024 * 1024)) ;;
    *)
      echo "Invalid target snapshot size unit: $raw" >&2
      return 2
      ;;
  esac

  printf '%s\n' "$((number * multiplier))"
}

if [ "${1:-}" = "--parse-size" ]; then
  parse_size_bytes "${2:-${TARGET_SNAPSHOT_MAX_SIZE:-0}}"
  exit $?
fi

is_true() {
  local value
  value="$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')"
  case "$value" in
    "1"|"true"|"yes"|"on") return 0 ;;
    *) return 1 ;;
  esac
}

write_output() {
  local name="$1"
  local value="$2"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf '%s=%s\n' "$name" "$value" >> "$GITHUB_OUTPUT"
  fi
}

format_bytes() {
  awk -v bytes="$1" '
    BEGIN {
      split("B KiB MiB GiB TiB", units, " ");
      value = bytes + 0;
      unit = 1;
      while (value >= 1024 && unit < 5) {
        value /= 1024;
        unit++;
      }
      if (unit == 1) {
        printf "%d %s", value, units[unit];
      } else {
        printf "%.1f %s", value, units[unit];
      }
    }
  ' 2>/dev/null || printf '%s B' "$1"
}

# Sum every regular file under `target/`, excluding `incremental/` (and
# optionally `*/build/*/out`). One Python process spawn — replaces the
# previous `find | while wc -c` loop that paid 30–50ms of Git-Bash
# process-spawn cost per file on Windows, dominating cleanup runtime
# (~80–90s on a typical workspace target/). See zccache#189.
#
# Only used in full mode; hot mode already gets its candidate bytes from
# `select-hot-target.py`'s JSON output.
snapshot_candidate_bytes() {
  local target="$1"
  local prune_build_out=0
  if is_true "${TARGET_PRUNE_BUILD_SCRIPT_OUT:-false}"; then
    prune_build_out=1
  fi
  python - "$target" "$prune_build_out" <<'PY'
import os
import sys

target = sys.argv[1]
prune_build_out = sys.argv[2] == "1"
total = 0
seen_inodes = set()
for root, dirs, files in os.walk(target):
    # Mirror the bash `find -prune` semantics: skip `incremental` and
    # optionally any `out` directory that sits under a `build/<pkg>/` path.
    pruned = []
    for d in dirs:
        if d == "incremental":
            pruned.append(d)
            continue
        if prune_build_out and d == "out":
            # `*/build/*/out` — match only if parent's parent is `build`.
            parent = os.path.basename(root)
            grandparent = os.path.basename(os.path.dirname(root))
            if grandparent == "build":
                pruned.append(d)
                continue
    for d in pruned:
        dirs.remove(d)
    for fname in files:
        try:
            st = os.stat(os.path.join(root, fname))
        except OSError:
            continue
        key = (st.st_dev, st.st_ino)
        if key in seen_inodes:
            continue
        seen_inodes.add(key)
        total += st.st_size
print(total)
PY
}

# Prune directories matching `find_kind` and emit `count bytes` summary.
# `bytes` is reported as 0 — the pre-#189 implementation walked each
# pruned directory before deletion to sum sizes, but the result was
# purely informational (never gated any decision) and the walk cost
# ~10–20s extra on Windows per cleanup. The `pruned-dirs` count is
# still accurate and remains the actionable signal.
prune_dirs() {
  local target="$1"
  local find_kind="$2"
  local count=0
  local dir

  if [ "$find_kind" = "incremental" ]; then
    while IFS= read -r -d '' dir; do
      rm -rf -- "$dir"
      count=$((count + 1))
    done < <(find "$target" -type d -name incremental -prune -print0 2>/dev/null)
  elif [ "$find_kind" = "build-out" ]; then
    while IFS= read -r -d '' dir; do
      rm -rf -- "$dir"
      count=$((count + 1))
    done < <(find "$target" -type d -path '*/build/*/out' -prune -print0 2>/dev/null)
  else
    echo "unknown prune kind: $find_kind" >&2
    return 2
  fi

  printf '%s 0\n' "$count"
}

emit_outputs() {
  write_output "snapshot-saved" "$SNAPSHOT_SAVED"
  write_output "snapshot-skipped-reason" "$SNAPSHOT_SKIPPED_REASON"
  write_output "snapshot-bytes" "$SNAPSHOT_BYTES"
  write_output "candidate-bytes" "$CANDIDATE_BYTES"
  write_output "pruned-dirs" "$PRUNED_DIRS"
  write_output "pruned-bytes" "$PRUNED_BYTES"
}

append_summary() {
  if [ -z "${GITHUB_STEP_SUMMARY:-}" ]; then
    return 0
  fi

  {
    echo "### zccache target snapshot"
    echo ""
    echo "- saved: $SNAPSHOT_SAVED"
    if [ -n "$SNAPSHOT_SKIPPED_REASON" ]; then
      echo "- skipped: $SNAPSHOT_SKIPPED_REASON"
    fi
    echo "- pruned directories: $PRUNED_DIRS"
    echo "- pruned bytes: $PRUNED_BYTES ($(format_bytes "$PRUNED_BYTES"))"
    echo "- candidate bytes: $CANDIDATE_BYTES ($(format_bytes "$CANDIDATE_BYTES"))"
    echo "- snapshot bytes: $SNAPSHOT_BYTES ($(format_bytes "$SNAPSHOT_BYTES"))"
  } >> "$GITHUB_STEP_SUMMARY"
}

finish_skipped() {
  SNAPSHOT_SAVED=false
  SNAPSHOT_SKIPPED_REASON="$1"
  emit_outputs
  append_summary
  echo "Skipped target snapshot: $SNAPSHOT_SKIPPED_REASON"
  exit 0
}

finish_too_large() {
  local bytes="$1"
  local message="target snapshot exceeds limit: $(format_bytes "$bytes") > $(format_bytes "$MAX_BYTES")"
  SNAPSHOT_SAVED=false
  SNAPSHOT_SKIPPED_REASON="target-too-large"
  emit_outputs
  append_summary
  if [ "$TOO_LARGE_POLICY" = "fail" ]; then
    echo "$message" >&2
    exit 1
  fi
  echo "$message; skipping save"
  exit 0
}

TARGET="${TARGET_DIR:-${TARGET:-target}}"
SNAPSHOT_DIR="${TARGET_SNAPSHOT_DIR:-$HOME/.zccache-target-meta}"
SNAPSHOT_MODE="$(printf '%s' "${TARGET_SNAPSHOT_MODE:-hot}" | tr '[:upper:]' '[:lower:]')"
HOT_MARKER_EPOCH="${TARGET_HOT_MARKER_EPOCH:-0}"
MAX_SIZE="${TARGET_SNAPSHOT_MAX_SIZE:-2GiB}"
TOO_LARGE_POLICY="$(printf '%s' "${TARGET_SNAPSHOT_TOO_LARGE:-skip}" | tr '[:upper:]' '[:lower:]')"

case "$SNAPSHOT_MODE" in
  "hot"|"full") ;;
  *)
    echo "Invalid target snapshot mode: $SNAPSHOT_MODE" >&2
    exit 2
    ;;
esac

case "$TOO_LARGE_POLICY" in
  "skip"|"fail") ;;
  *)
    echo "Invalid target snapshot too-large policy: $TOO_LARGE_POLICY" >&2
    exit 2
    ;;
esac

MAX_BYTES="$(parse_size_bytes "$MAX_SIZE")"
SNAPSHOT_TAR="$SNAPSHOT_DIR/target-meta.tar"
SNAPSHOT_SAVED=false
SNAPSHOT_SKIPPED_REASON=""
SNAPSHOT_BYTES=0
CANDIDATE_BYTES=0
PRUNED_DIRS=0
PRUNED_BYTES=0

rm -rf -- "$SNAPSHOT_DIR"
mkdir -p -- "$SNAPSHOT_DIR"

if [ ! -d "$TARGET" ]; then
  finish_skipped "missing-target-dir"
fi

if is_true "${TARGET_PRUNE_INCREMENTAL:-true}"; then
  read -r count bytes < <(prune_dirs "$TARGET" "incremental")
  PRUNED_DIRS=$((PRUNED_DIRS + count))
  PRUNED_BYTES=$((PRUNED_BYTES + bytes))
fi

if is_true "${TARGET_PRUNE_BUILD_SCRIPT_OUT:-false}"; then
  read -r count bytes < <(prune_dirs "$TARGET" "build-out")
  PRUNED_DIRS=$((PRUNED_DIRS + count))
  PRUNED_BYTES=$((PRUNED_BYTES + bytes))
fi

# Full mode needs a whole-`target/` walk to gate the size check below.
# Hot mode does its own walk inside `select-hot-target.py` and rebinds
# `CANDIDATE_BYTES` from its JSON stats — skip the bash-side walk
# entirely for hot mode to avoid duplicating the work (#189).
if [ "$SNAPSHOT_MODE" = "full" ]; then
  CANDIDATE_BYTES="$(snapshot_candidate_bytes "$TARGET")"
  if [ "$MAX_BYTES" -gt 0 ] && [ "$CANDIDATE_BYTES" -gt "$MAX_BYTES" ]; then
    finish_too_large "$CANDIDATE_BYTES"
  fi
fi

if [ "$SNAPSHOT_MODE" = "hot" ]; then
  LIST_FILE="$SNAPSHOT_DIR/hot-target-files.list"
  selector_args=(
    "$SCRIPT_DIR/select-hot-target.py"
    --target "$TARGET"
    --marker-epoch "$HOT_MARKER_EPOCH"
    --list-file "$LIST_FILE"
  )
  if is_true "${TARGET_PRUNE_BUILD_SCRIPT_OUT:-false}"; then
    selector_args+=(--prune-build-script-out)
  fi
  HOT_STATS="$(python "${selector_args[@]}")"
  CANDIDATE_BYTES="$(python - "$HOT_STATS" <<'PY'
import json
import sys
print(json.loads(sys.argv[1])["selected_bytes"])
PY
)"
  if [ "$MAX_BYTES" -gt 0 ] && [ "$CANDIDATE_BYTES" -gt "$MAX_BYTES" ]; then
    finish_too_large "$CANDIDATE_BYTES"
  fi
  if [ ! -s "$LIST_FILE" ]; then
    finish_skipped "no-hot-target-files"
  fi
  tar_command=(tar -cf "$SNAPSHOT_TAR" -C "$TARGET" --null -T "$LIST_FILE")
else
  tar_args=(--exclude='incremental' --exclude='*/incremental' -C "$TARGET" .)
  if is_true "${TARGET_PRUNE_BUILD_SCRIPT_OUT:-false}"; then
    tar_args=(--exclude='incremental' --exclude='*/incremental' --exclude='*/build/*/out' -C "$TARGET" .)
  fi
  tar_command=(tar -cf "$SNAPSHOT_TAR" "${tar_args[@]}")
fi

if ! "${tar_command[@]}" 2>/dev/null; then
  rm -f -- "$SNAPSHOT_TAR"
  finish_skipped "tar-failed"
fi

SNAPSHOT_BYTES="$(wc -c < "$SNAPSHOT_TAR" | tr -d '[:space:]')"
if [ "$MAX_BYTES" -gt 0 ] && [ "$SNAPSHOT_BYTES" -gt "$MAX_BYTES" ]; then
  rm -f -- "$SNAPSHOT_TAR"
  finish_too_large "$SNAPSHOT_BYTES"
fi

SNAPSHOT_SAVED=true
emit_outputs
append_summary
echo "Saved target snapshot ($(format_bytes "$SNAPSHOT_BYTES")); pruned $PRUNED_DIRS dirs ($(format_bytes "$PRUNED_BYTES"))"
