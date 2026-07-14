#!/usr/bin/env bash
set -euo pipefail
z=/target/debug/zccache
cargo test -p zccache --lib --bins --features zccache-bin,daemon-bin,download-bin,download-daemon-bin,fingerprint-bin,stamp-bin,ci-bin,crash-tools,tokio-console,test-support -- --test-threads=1
output="$($z gha-cache status 2>&1 || true)"
echo "$output"
echo "$output" | grep -qi 'gha cache'
unset ACTIONS_CACHE_URL ACTIONS_RUNTIME_TOKEN
output="$($z gha-cache save --key local-pre-pr --path /tmp 2>&1 || true)"
echo "$output"
echo "$output" | grep -Eqi 'not running|not available'
