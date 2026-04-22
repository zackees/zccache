#!/usr/bin/env bash
set -euo pipefail

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

sum_file_bytes_from_find() {
  local total=0
  local file size
  while IFS= read -r -d '' file; do
    size="$(wc -c < "$file" | tr -d '[:space:]')" || size=0
    total=$((total + size))
  done
  printf '%s\n' "$total"
}

directory_bytes() {
  local dir="$1"
  find "$dir" -type f -print0 2>/dev/null | sum_file_bytes_from_find
}

snapshot_candidate_bytes() {
  local target="$1"
  if is_true "${TARGET_PRUNE_BUILD_SCRIPT_OUT:-false}"; then
    find "$target" \
      \( -type d -name incremental -o -type d -path '*/build/*/out' \) -prune \
      -o -type f -print0 2>/dev/null | sum_file_bytes_from_find
  else
    find "$target" \
      -type d -name incremental -prune \
      -o -type f -print0 2>/dev/null | sum_file_bytes_from_find
  fi
}

prune_dirs() {
  local target="$1"
  local find_kind="$2"
  local count=0
  local bytes=0
  local dir dir_bytes

  if [ "$find_kind" = "incremental" ]; then
    while IFS= read -r -d '' dir; do
      dir_bytes="$(directory_bytes "$dir")"
      rm -rf -- "$dir"
      count=$((count + 1))
      bytes=$((bytes + dir_bytes))
    done < <(find "$target" -type d -name incremental -prune -print0 2>/dev/null)
  elif [ "$find_kind" = "build-out" ]; then
    while IFS= read -r -d '' dir; do
      dir_bytes="$(directory_bytes "$dir")"
      rm -rf -- "$dir"
      count=$((count + 1))
      bytes=$((bytes + dir_bytes))
    done < <(find "$target" -type d -path '*/build/*/out' -prune -print0 2>/dev/null)
  else
    echo "unknown prune kind: $find_kind" >&2
    return 2
  fi

  printf '%s %s\n' "$count" "$bytes"
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
MAX_SIZE="${TARGET_SNAPSHOT_MAX_SIZE:-2GiB}"
TOO_LARGE_POLICY="$(printf '%s' "${TARGET_SNAPSHOT_TOO_LARGE:-skip}" | tr '[:upper:]' '[:lower:]')"

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

CANDIDATE_BYTES="$(snapshot_candidate_bytes "$TARGET")"
if [ "$MAX_BYTES" -gt 0 ] && [ "$CANDIDATE_BYTES" -gt "$MAX_BYTES" ]; then
  finish_too_large "$CANDIDATE_BYTES"
fi

tar_excludes=(--exclude='incremental' --exclude='*/incremental')
if is_true "${TARGET_PRUNE_BUILD_SCRIPT_OUT:-false}"; then
  tar_excludes+=(--exclude='*/build/*/out')
fi

if ! tar "${tar_excludes[@]}" -cf "$SNAPSHOT_TAR" -C "$TARGET" . 2>/dev/null; then
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
