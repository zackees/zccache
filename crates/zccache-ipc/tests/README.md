# `zccache-ipc` integration tests

Each `.rs` file here compiles to its own test binary that exercises the
public surface of `zccache-ipc`.

- `timeout.rs` — `recv` timeout behavior: unbounded default, opt-in
  `set_recv_timeout`, per-call `recv_with_timeout`, and the
  peer-death-is-not-timeout invariant.
