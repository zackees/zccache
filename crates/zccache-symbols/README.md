# zccache-symbols

Release-build marker, symbol cache, and symbol-archive fetcher.

## Wire format

Release binaries carry a fixed 96-byte footer appended after every other
section the OS loader looks at. PE/ELF/Mach-O all tolerate trailing
bytes past what their headers describe, so the OS loader ignores the
footer and just reads it back at runtime via `current_exe()`.

```
[  0.. 40]  git SHA hex (40 chars, NUL-padded if short)
[ 40.. 56]  semver version ("1.7.2"), NUL-padded
[ 56.. 88]  rustc target triple, NUL-padded
[ 88.. 96]  build timestamp (u64 LE, unix seconds)
[ 96..120]  reserved zeros (forward-compat)
[120..128]  magic = b"ZCCSYMv1"
```

Absence of the magic at byte 88..96 means "this is a dev build" — the
caller skips the auto-fetch path entirely and tells the user to use
local `target/release/*.{pdb,dwp,dSYM}` instead.

## Stamping

`zccache-stamp` (binary in this crate) is run on the release runner
after stripping but before archiving. Cross-compile-safe: it doesn't
execute the target binary, just appends bytes.

## Lazy fetch

The fetch itself lives in `zccache-cli::symbols::install` (cross-process
locked, archive-cached, atomic-rename). `zccache symbolicate <dump>`
reads the marker from `current_exe()`, calls `install()` with a prefix
of `<cache>/symbols/<v>-<triple>/`, then writes the sidecar.

## Placement

Each crash dump gets a `<dump>.symref` sidecar next to it containing
the absolute path to the extracted symbol directory. Symbols live ONCE
in `<cache>/symbols/` and are referenced per-crash — no duplication.
