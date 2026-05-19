# persist-rust-bench src

Trivial `main.rs` that touches each dependency once so the linker keeps
them in the final binary. The point isn't the runtime behaviour — it's
the cold-compile artifact set produced from the dep tree.
