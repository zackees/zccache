# zccache-symbols source

- `lib.rs` — crate root, re-exports
- `marker.rs` — read/write the 128-byte release footer
- `cache.rs` — symbol cache directory layout and `.symref` sidecars
- `bin/stamp.rs` — `zccache-stamp` CI helper that appends the footer

The actual symbol-archive download lives in `zccache-cli::symbols`, not
here.
