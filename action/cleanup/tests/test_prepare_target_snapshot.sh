#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SCRIPT="$ROOT/action/cleanup/prepare-target-snapshot.sh"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_file_exists() {
  [ -f "$1" ] || fail "expected file to exist: $1"
}

assert_file_missing() {
  [ ! -e "$1" ] || fail "expected path to be missing: $1"
}

assert_contains() {
  local file="$1"
  local text="$2"
  grep -Fq "$text" "$file" || fail "expected $file to contain: $text"
}

assert_not_contains() {
  local file="$1"
  local text="$2"
  if grep -Fq "$text" "$file"; then
    fail "expected $file not to contain: $text"
  fi
}

make_file() {
  local path="$1"
  local bytes="$2"
  mkdir -p "$(dirname "$path")"
  python - "$path" "$bytes" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
size = int(sys.argv[2])
path.write_bytes(b"x" * size)
PY
}

new_tmp() {
  local dir
  dir="$(mktemp -d)"
  echo "$dir"
}

tar_listing() {
  local tar_path="$1"
  local output="$2"
  tar tf "$tar_path" | sed 's#^\./##' > "$output"
}

run_prepare() {
  TARGET_DIR="$1" \
  TARGET_SNAPSHOT_DIR="$2" \
  TARGET_SNAPSHOT_MAX_SIZE="${3:-0}" \
  TARGET_SNAPSHOT_TOO_LARGE="${4:-skip}" \
  TARGET_PRUNE_INCREMENTAL="${5:-true}" \
  TARGET_PRUNE_BUILD_SCRIPT_OUT="${6:-false}" \
  GITHUB_OUTPUT="$7" \
  bash "$SCRIPT"
}

test_parse_size() {
  [ "$(bash "$SCRIPT" --parse-size 2GiB)" = "2147483648" ] || fail "2GiB parse failed"
  [ "$(bash "$SCRIPT" --parse-size 16kb)" = "16384" ] || fail "16kb parse failed"
  [ "$(bash "$SCRIPT" --parse-size unlimited)" = "0" ] || fail "unlimited parse failed"
}

test_prunes_incremental_and_excludes_from_tar() {
  local tmp target snapshot output listing
  tmp="$(new_tmp)"
  target="$tmp/target"
  snapshot="$tmp/snapshot"
  output="$tmp/output"
  listing="$tmp/listing"

  make_file "$target/debug/deps/libfoo.rlib" 8
  make_file "$target/debug/incremental/foo/s-cache.bin" 64
  make_file "$target/release/incremental/bar/s-cache.bin" 32

  run_prepare "$target" "$snapshot" 0 skip true false "$output"

  assert_file_missing "$target/debug/incremental"
  assert_file_missing "$target/release/incremental"
  assert_file_exists "$snapshot/target-meta.tar"
  assert_contains "$output" "snapshot-saved=true"
  assert_contains "$output" "pruned-dirs=2"

  tar_listing "$snapshot/target-meta.tar" "$listing"
  assert_contains "$listing" "debug/deps/libfoo.rlib"
  assert_not_contains "$listing" "incremental"
}

test_optionally_prunes_build_script_out() {
  local tmp target snapshot output listing
  tmp="$(new_tmp)"
  target="$tmp/target"
  snapshot="$tmp/snapshot"
  output="$tmp/output"
  listing="$tmp/listing"

  make_file "$target/debug/build/libz-sys-abc/out/native/libz.a" 32
  make_file "$target/debug/build/libz-sys-abc/build-script-build" 8
  make_file "$target/debug/deps/libz_sys.rlib" 8

  run_prepare "$target" "$snapshot" 0 skip true true "$output"

  assert_file_missing "$target/debug/build/libz-sys-abc/out"
  assert_file_exists "$snapshot/target-meta.tar"

  tar_listing "$snapshot/target-meta.tar" "$listing"
  assert_not_contains "$listing" "debug/build/libz-sys-abc/out"
  assert_contains "$listing" "debug/build/libz-sys-abc/build-script-build"
  assert_contains "$listing" "debug/deps/libz_sys.rlib"
}

test_oversize_skip_does_not_create_tar() {
  local tmp target snapshot output
  tmp="$(new_tmp)"
  target="$tmp/target"
  snapshot="$tmp/snapshot"
  output="$tmp/output"

  make_file "$target/debug/deps/libhuge.rlib" 64

  run_prepare "$target" "$snapshot" 16B skip true false "$output"

  assert_file_missing "$snapshot/target-meta.tar"
  assert_contains "$output" "snapshot-saved=false"
  assert_contains "$output" "snapshot-skipped-reason=target-too-large"
}

test_oversize_fail_returns_nonzero() {
  local tmp target snapshot output
  tmp="$(new_tmp)"
  target="$tmp/target"
  snapshot="$tmp/snapshot"
  output="$tmp/output"

  make_file "$target/debug/deps/libhuge.rlib" 64

  if run_prepare "$target" "$snapshot" 16B fail true false "$output"; then
    fail "expected oversize fail policy to return non-zero"
  fi

  assert_file_missing "$snapshot/target-meta.tar"
  assert_contains "$output" "snapshot-saved=false"
  assert_contains "$output" "snapshot-skipped-reason=target-too-large"
}

test_parse_size
test_prunes_incremental_and_excludes_from_tar
test_optionally_prunes_build_script_out
test_oversize_skip_does_not_create_tar
test_oversize_fail_returns_nonzero

echo "prepare-target-snapshot tests passed"
