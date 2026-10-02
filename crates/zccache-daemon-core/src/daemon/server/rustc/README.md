# server::rustc submodules

- `link_key.rs` — linker key material for rustc products whose bytes the
  linker shapes: the Dylint lint cdylib and an admitted `--test` harness
  (#1550). Folds the explicit linker's identity (or `default`) and every
  `-C link-arg(s)` into the context key.
