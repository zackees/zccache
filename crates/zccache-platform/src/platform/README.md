# platform — neutral facades

Five capability namespaces. Neutral code only: `Path`/`PathBuf`, primitive
types, platform-owned primitive errors — never zccache product types and
never native handles. Concrete implementations live in the private
`platform_{win,linux,macos}` trees and are reached via `crate::platform_imp`.

| Facade | Owns |
|---|---|
| `process` | spawn/detach mechanics, containment, priority application, PID liveness, executable-image lookup, CPU probes, stdio detach, exit/signal interpretation, jobserver pipes |
| `fs` | file/volume identity, same-file comparison, change markers, hard-link counts, permissions, atomic replace, symlink/reparse handling, path-key normalization, clone/reflink/positioned I/O, free-space probes |
| `ipc` | neutral Stream/Listener/Endpoint/PeerIdentity over local transport; owner-only socket/pipe security; native endpoint retirement |
| `executable` | native suffixes, PATH/PATHEXT lookup, running-image discovery, runnable-image materialization |
| `host` | OS/arch facts, home/runtime directories, elevation, Defender primitives, host resource/pressure facts |
