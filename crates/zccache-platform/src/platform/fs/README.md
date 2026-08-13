# platform::fs — neutral filesystem facade

| Leaf | Owns |
|---|---|
| `identity` | `FileIdentity` (opaque, Eq+Hash), `ChangeMarker` (Option when the host can prove one), `file_identity`, `same_file`, `change_marker` |
| `links` | `LinkKind` (regular/symlink/reparse without native attribute bits), `hard_link_count`, `classify` |
| `permissions` | `ensure_dir_private`, `create_dir_all_private`, `set_readonly`, `make_writable`, `make_executable` |
| `replace` | `atomic_replace` (atomic destination replacement; the caller owns temp naming and retries) |
| `volume` | `VolumeIdentity` (opaque, Eq+Hash), `volume_identity`, stateless capability facts (hard-link limit, file-id width) |
| `path` | host path-key normalization: extended-prefix stripping, case folding, macOS `/private/var` handling, MSYS conversion |

Concrete implementations live in `platform_{win,linux,macos}/fs/…` and are
selected once by the crate-root `cfg_select!`. Neutral types keep their
native representations private; only neutral constructors/accessors cross
the facade.
