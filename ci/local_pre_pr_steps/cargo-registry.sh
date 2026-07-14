#!/usr/bin/env bash
set -euo pipefail
z=/target/debug/zccache
hash1="$($z cargo-registry hash --lockfile Cargo.lock)"
hash2="$($z cargo-registry hash --lockfile Cargo.lock)"
test "${#hash1}" -eq 16
test "$hash1" = "$hash2"
if $z cargo-registry hash --lockfile nonexistent.lock >/dev/null 2>&1; then exit 1; fi
cargo fetch
$z cargo-registry save --key local-pre-pr
root="$($z cache-root)"
test -f "$root/cargo-registry/local-pre-pr.tar.gz"
rm -rf "$CARGO_HOME/registry/cache" "$CARGO_HOME/registry/index"
$z cargo-registry restore --key local-pre-pr
test "$(find "$CARGO_HOME/registry" -type f 2>/dev/null | wc -l)" -gt 0
$z cargo-registry clean
test ! -e "$root/cargo-registry/local-pre-pr.tar.gz"
