# zccache-platform

The dependency-leaf crate that owns **host-platform mechanics** for the whole
workspace (zccache#1365). Host selection happens exactly once, in
[`src/lib.rs`](src/lib.rs), through `std::cfg_select!` — no fallback arm, no
Unix selector, no runtime platform object.

```
src/lib.rs            the one selector: cfg_select! { windows | linux | macos }
src/platform.rs       neutral facade root (no host cfg, ever)
src/platform/         process, fs, ipc, executable, host — neutral types/ops only
src/platform_win.rs   concrete Windows tree (private; host cfg + native APIs live here)
src/platform_linux.rs concrete Linux tree (private)
src/platform_macos.rs concrete macOS tree (private)
```

## Rules

- **One selector.** Ordinary production code calls `crate::platform` and never
  names a concrete host implementation. Unsupported host OSes fail
  compilation at the selector.
- **Leaf.** No dependency on any `zccache-*` crate and no product types
  (`NormalizedPath`, `Config`, protocol messages, audit events, …). Callers
  translate primitive results into product types and diagnostics.
- **Host is not compiler target.** Compiler/build-target decisions
  (`rustc --target`, MSVC/GNU linkers, output extensions from an explicit
  triple) stay in zccache-compiler; this crate only answers "what OS is this
  zccache process running on?".
- The `enforce_platform_boundary` Dylint keeps every other production source
  free of host cfg and native imports (transitional exact-occurrence baseline
  included).
- On publish, `ci/publish_amalgamate.py` copies this crate into the published
  `zccache` crate as a private `platform` module and rewrites
  `zccache_platform::` paths to `crate::platform::`. It is not a public
  crates.io API.

See `docs/architecture/portability.md` for the full boundary contract.
