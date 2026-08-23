# response_file

Compiler-specific formatting for response files — the `@file` argument
indirection used when a command line would otherwise exceed the OS limit.

Each compiler family quotes and escapes differently, so a single formatter
would silently corrupt arguments for one of them.

## Layout

- `format.rs` — `gnu` plus the `*_if_safe` variants (`gnu_if_safe`,
  `msvc_if_safe`, `rustc_if_safe`). The `_if_safe` forms return `None` rather
  than emitting a response file when an argument cannot be represented
  losslessly in that family's grammar, so the caller falls back to a direct
  command line instead of shipping a mangled one.
