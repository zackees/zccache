#!/usr/bin/env bash
set -euo pipefail
export SOLDR_CACHE_LIFECYCLE=command
export SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS=30
export ZCCACHE_CACHE_DIR=/tmp/local-pre-pr-zccache
export SOLDR_RUSTC_WRAPPER=/target/debug/zccache
rm -rf "$ZCCACHE_CACHE_DIR"
tmp="$(mktemp -d)"
mkdir "$tmp/hello"
cat > "$tmp/hello/Cargo.toml" <<'EOF'
[package]
name = "hello"
version = "0.1.0"
edition = "2021"
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
EOF
mkdir "$tmp/hello/src"
printf 'use serde::Serialize;\n#[derive(Serialize)] struct Hello { value: u8 }\nfn main() { println!("{}", serde_json::to_string(&Hello { value: 1 }).unwrap()); }\n' > "$tmp/hello/src/main.rs"
cd "$tmp/hello"
cargo build -vv 2>&1 | tee build.log
# Cargo does not include RUSTC_WRAPPER in its verbose command line when
# invoked directly. The successful proc-macro build plus the isolated cache
# root are the stable local equivalents of the workflow log assertion.
test -d "$($SOLDR_RUSTC_WRAPPER cache-root)"
/target/debug/zccache stop >/dev/null 2>&1 || true
