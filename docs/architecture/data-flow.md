# Data Flow

Step-by-step traces of the three main execution paths through zccache.

For component details see [overview.md](overview.md). For IPC specifics see [ipc.md](ipc.md).

---

## Cache Hit Path

There are two IPC modes:

- **Ephemeral mode** (drop-in wrapper, no `ZCCACHE_SESSION_ID`): CLI sends a single
  `Request::CompileEphemeral` message. The daemon creates an internal session,
  compiles, ends the session, and returns the result — **1 IPC roundtrip**.
- **Session mode** (`ZCCACHE_SESSION_ID` set by a build system integration):
  CLI sends `Request::Compile` within an existing session — also 1 roundtrip,
  but the session was created separately.

```
User invokes:  zccache clang++ -c foo.c -o foo.o

1. CLI parses argv. Determines: compiler=clang++, source=foo.c, output=foo.o.
   This is a single-source compilation — cacheable.

2. CLI calls connect().
   a. Compute socket path: $XDG_RUNTIME_DIR/zccache/sock (Unix)
      or \\.\pipe\zccache-{username} (Windows).
   b. Attempt connect. If refused or socket missing:
      - Check lock file. If lock file exists and process alive, retry briefly.
      - Otherwise, clean stale socket/lock, fork/spawn daemon, wait for
        socket to appear, connect.

3. CLI sends Request::CompileEphemeral { client_pid, working_dir, compiler,
   args, cwd, env } over IPC. (Single roundtrip — session start, compile,
   and session end are handled internally by the daemon.)

4. Daemon IPC server receives request, spawns a tokio task.

5. Daemon re-parses args server-side to extract canonical info.
   Resolves compiler path to absolute.

6. Daemon computes compiler identity hash:
   a. Check metadata cache for compiler binary. If High confidence and
      content_hash is Some, use it.
   b. Otherwise, stat the compiler binary, update metadata entry, hash
      the file, store in metadata cache at High confidence.

7. Daemon computes source content hash:
   a. Check metadata cache for foo.c. Suppose confidence is Medium
      (watcher says unchanged).
   b. Medium is not High — stat the file. Compare (mtime, size, file_id)
      with cached entry.
   c. Match: promote to High confidence, use cached content_hash if present.
      No match: re-hash file, update entry at High.

8. Daemon computes dependency hash:
   (MVP: run preprocessor to get dependency content hash. Future: use
   cached per-header hashes.)

9. Daemon computes cache key = blake3(compiler_id, sorted_args,
   sorted_env, source_hash, dep_hash).

10. Daemon queries ArtifactStore::lookup(key).
    a. redb index lookup by key — found, returns artifact directory path
       and metadata.
    b. Verify artifact directory exists and manifest is intact.
    c. Update last-access-time in redb index.

11. Daemon copies cached output files to the requested output paths
    (e.g., copies cached object file to foo.o).

12. Daemon sends Response::CacheHit { exit_code: 0, stdout, stderr }
    over IPC.

13. CLI receives response. Writes stdout/stderr to its own stdout/stderr.
    Exits with the cached exit code.
```

## Cache Miss Path

```
Steps 1–9: identical to cache hit path.

10. Daemon queries ArtifactStore::lookup(key) — not found.

11. Daemon calls CompilerManager::run_compiler(compiler, args, cwd, env).
    a. Spawns the real compiler as a child process via tokio::process::Command.
    b. Waits for completion, captures stdout, stderr, exit code.

12. If exit code != 0, daemon sends Response::CacheMiss { exit_code,
    stdout, stderr }. Does NOT cache failed compilations. Done.

13. If exit code == 0, daemon stores the artifact:
    a. Create temp directory under {cache_root}/tmp/{random}.
    b. Copy output files into temp dir.
    c. Write manifest.json into temp dir.
    d. Compute artifact content hash (the cache key).
    e. Rename temp dir to {cache_root}/artifacts/{hash[0..2]}/{hash[2..4]}/{hash}.
       Atomic on same filesystem.
    f. Insert entry into redb index with current timestamp as last-access-time.
    g. If total cache size exceeds max, trigger async eviction.

14. Daemon sends Response::CacheMiss { exit_code: 0, stdout, stderr }.

15. CLI receives response. Output files already exist on disk (the real
    compiler wrote them). CLI writes stdout/stderr, exits with exit code.
```

## Non-Cacheable Invocation Passthrough

```
User invokes:  zccache-cc foo.c bar.c -o program   (linking, multiple sources)

1. CLI parses argv. Determines this is a link invocation or multi-source
   compilation — not cacheable.

2. CLI does NOT contact the daemon.

3. CLI execs the underlying compiler directly:
   a. Determine real compiler path (from PATH, skipping zccache wrappers).
   b. execvp(compiler, original_args).

4. CLI process is replaced by the compiler. Exit code propagates to the
   build system.
```

Non-cacheable patterns detected by the CLI:
- No `-c` flag (linking invocation).
- Multiple source files.
- `-E` / `-M` / `-MM` (preprocessing / dependency generation only).
- `-` as input (stdin source).
- Unrecognized compiler.

## Rustc Cache-Key Specifics (zccache#1021)

The rustc lane shares the pipeline above but has four key-scope rules of
its own:

- **Env-deps are cache inputs.** rustc records every `env!()` /
  `option_env!()` read as a `# env-dep:NAME[=value]` line in its
  dep-info. The daemon stores the name set per context
  (`DepGraph::rustc_env_deps`, persisted in the depgraph snapshot) and
  folds the blake3 hash of each CURRENT value into the artifact key
  (`fold_rustc_env_deps_into_artifact_key`). A changed
  `cargo:rustc-env` value (vergen's `VERGEN_GIT_SHA`, shadow-rs, etc.)
  therefore forces a recompile instead of serving an rlib with the old
  value baked in. Unset is a distinct variant from every set value.
  Contexts with no env-deps (the overwhelmingly common case) keep
  byte-identical keys with prior releases. `CARGO_*` values are
  additionally fingerprinted request-side (see
  `request_env_fingerprint_vars`).
- **`-C incremental` is excluded from the key** and allowed on misses.
  Deliberate divergence from sccache (which refuses incremental):
  cargo passes incremental on every dev-profile compile, and the
  emitted rlib/rmeta interface (SVH) is stable even though CGU
  partitioning may differ. See the note in
  `zccache-compiler/src/parse_rustc.rs`.
- **Cacheable crate types are `lib`, `rlib`, `staticlib`, `proc-macro`,
  `bin`.** `dylib`/`cdylib` are deliberately not cached (platform
  linker state is not modeled) — PyO3/maturin `cdylib` final artifacts
  recompile every time while their rlib deps still hit.
- **Native libraries in link steps are a documented blind spot.**
  `bin`/`staticlib` units linking system libraries via `-L`/`-l` do not
  content-hash the resolved library bytes (matching sccache). An
  upgraded system library with an otherwise-identical invocation can
  serve a stale binary; the accepted trade-off avoids per-link hashing
  of large system libraries. Revisit if a real-world stale-bin report
  lands.

## Nested Dylint Driver Caching

Dylint invokes Rust through a two-level compiler command:

```text
dylint-driver <rustc> <rustc arguments...>
```

zccache recognizes only the exact `dylint-driver` executable basename. It
preserves the outer executable and nested argv layout, while parsing the
arguments after the inner rustc path as an ordinary Rust compilation. When
automatic path remapping is enabled and a mapping is needed, its rustc flag is
inserted after the inner compiler path so the nested command shape remains
valid.

The cache identity adds the content identities of the outer driver, inner
rustc, and the name plus content of every dynamic library listed in the JSON
`DYLINT_LIBS` array (the name controls Dylint's injected `dylint_lib` cfg). It
also includes `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, and all output-affecting
`DYLINT_*` variables. Dylint's diagnostic-link control
`CLIPPY_DISABLE_DOCS_LINKS` is keyed as well. The daemon folds those inputs into an internal
`ZCCACHE_DYLINT_CACHE_INPUT_HASH` salt used by both the request cache and Rust
artifact context; the salt is never replayed to the driver process. Executable
and library identities use the daemon's metadata-backed identity cache, so a
warm lint unit does not re-hash a large rustc binary.

The real Dylint driver may cause rustc dep-info to record `DYLINT_LIBS` as an
environment dependency. Its raw value contains checkout-local absolute library
paths, so the daemon resolves that recorded dependency through
`ZCCACHE_DYLINT_CACHE_INPUT_HASH` for lookup, freshness checks, and
publication. This preserves rustc's env-dependency invalidation while making
equivalent sibling worktrees depend on the already-validated library content
identity instead of path spelling. Ordinary rustc requests and Dylint requests
that did not complete input preparation continue to use the raw env value.

Missing, malformed, empty, or unhashable `DYLINT_LIBS` state disables caching
for that invocation. The driver still runs with its original argv,
environment, stdin, stdout, stderr, and exit status, and stderr receives a
visible `Dylint cache disabled` reason. Successful misses store the driver's
diagnostics, so a hit replays the same stdout and stderr rather than suppressing
lint output.

Dylint's earlier lint-library bootstrap is a separate, narrowly modeled Rust
`cdylib` lane on Linux and macOS. General `cdylib` and every Windows `cdylib`
remain non-cacheable. The narrow lane requires the isolated
`target/dylint/libraries/...` output tree, host compilation, no extra filename,
and `dylint-link` as the linker. Its key includes the linker binary and link
arguments. The artifact set contains both rustc's declared dynamic library and
the toolchain-qualified sidecar that `dylint-link` byte-copies for Dylint to
load. Missing package/toolchain identity fails back to the direct compiler
path, so a hit cannot silently omit the sidecar.

This cache is separate from Cargo incremental compilation:

- Cargo incremental state accelerates work inside one target directory and
  toolchain invocation.
- zccache restores whole Dylint-produced Rust artifacts across clean target
  directories. Equivalent worktrees can share those artifacts when effective
  path remapping normalizes their roots and source, toolchain, driver,
  libraries, and output-affecting environment all match.
- Different targets or different Dylint/rustc identities intentionally do not
  share artifacts.
