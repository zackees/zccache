# Cargo configuration

The `dylint-link` linker wrapper emits the toolchain-suffixed lint library
name required by Dylint. Keep `RUSTFLAGS` unset when building this crate;
it overrides these configured rustflags.
