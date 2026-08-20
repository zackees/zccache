# IPC integration-style unit tests

This directory holds coherent test groups split from `../mod_tests.rs` so the
main IPC test module remains within the repository source-file size limit.

`full_family.rs` exercises prost-first full-family requests, structured
old-daemon fallback, and the no-replay guarantee for application errors.
