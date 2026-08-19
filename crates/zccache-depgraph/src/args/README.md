# Dependency argument helpers

This directory contains focused helpers for GNU-compatible compiler argument
parsing. `dep_flags.rs` distinguishes driver dependency flags from options
forwarded directly to the preprocessor, whose value arity differs.
