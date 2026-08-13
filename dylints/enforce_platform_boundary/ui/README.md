# UI fixtures

- `allowed.rs` — host-independent cfgs and plain code: must produce no
  diagnostics.
- `disallowed.rs` — the forbidden syntax families from zccache#1365; every
  construct must produce one `enforce_platform_boundary` error:
  private `cfg!(windows)`, `#[cfg(unix)]` on private and public items,
  `#[cfg_attr(target_os = "windows", …)]`, `target_arch`/`target_env`/
  `target_family` variants, direct `platform_win`/`platform_imp` references,
  and imports of `std::os::windows`, `std::os::unix`, `windows_sys`, and
  `libc`.

The fixtures stay host-independent:

- `libc` and `windows_sys` are shadowed by local modules — the lint matches
  pre-expansion path *names*, so a shadow module exercises the same check
  without linking the real crates (and `std` itself cannot be shadowed).
- The real `std::os::{windows,unix}` imports are gated on the cfg where they
  exist so the fixture compiles on every host; the pre-expansion lint still
  sees and flags them everywhere, including the inactive branch.

Keep the substring `ui` out of lint messages: compiletest normalizes fixture
paths and rewrites `ui` to `$DIR` mid-word.
