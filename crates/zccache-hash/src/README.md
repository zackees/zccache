# Source

Source files for `zccache-hash`.

`request_fingerprint.rs` streams the daemon's v2 request-key encoding to a
fallible sink. Callers retain path normalization and environment-selection
policy; this module does not make the crate's native dependency graph portable.
