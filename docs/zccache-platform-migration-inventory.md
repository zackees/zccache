# `zccache-platform` migration inventory

This is the checked-in Phase-0 ownership inventory for kernal-api#5. It is a
source inventory and migration order, not validation evidence. The current
`zccache-platform` has been removed from the release graph after the listed
consumers own their product policy and directly use kernal-api's capabilities.
No row authorizes a wire-format, endpoint, cache-key, timeout, or
process-lifetime change.

## Inventory rules

- **Reuse** means the symbol is already an exact kernal-api re-export or
  delegation. Move its import to the consuming crate; do not add a second
  zccache wrapper merely to retain the old crate boundary.
- **Retain product policy** means the code moves to the named zccache owner.
  It may consume kernal-api capabilities, but must not reintroduce a native
  import, host `cfg`, Tokio concrete type in a public signature, or a direct
  running-process dependency.
- **Compatibility adapter** means the public zccache protocol or Tokio I/O
  shape still requires an adapter. The adapter moves beside that protocol,
  rather than becoming a generic kernal-api API.
- Test-only process fixtures move to `zccache-test-support` or a test module;
  they are not a reason to retain the production platform crate.

## Public surface mapping

| Existing `zccache_platform` surface | Finding | Destination and closure condition |
| --- | --- | --- |
| `executable::{current_image, native_name, find_in_paths, find_on_path, native_library_name, stem_matches, unlock_for_replacement, images_equal}` | Exact kernal-api executable/filesystem capabilities | **Reuse.** Import from `kernal_api::platform::{executable,fs}` at each caller. The platform module disappears once no non-test caller imports it. |
| `executable::clang_library_candidates` | Ordered LLVM/Homebrew/Windows candidate list is compiler-product policy, not host discovery | **Retain product policy** in `zccache-compiler` (or its existing CLI owner). Preserve list order and wording; use `kernal_api::platform::host::process_target()` only for host selection. |
| `fs::{FileIdentity,file_identity,same_file,ChangeMarker,change_marker}` | Exact KA opaque identity and optional change-marker contract | **Reuse.** Direct callers import `kernal_api::platform::fs::{path_file,FileChangeMarker}`. |
| `fs::{sync_directory,open_shared_append}` | Exact durability compatibility aliases | **Reuse.** Direct callers import KA filesystem primitives. |
| `fs::{LinkKind,classify,hard_link_count,symlink_file}` | Exact no-follow link/reparse contract | **Reuse.** Direct callers import KA filesystem primitives. |
| `fs::{atomic_replace,rename_without_replace,replace_with_delete_fallback,install_directory,is_lock_contention,is_transient_share_error}` | Exact KA replacement compatibility namespace | **Reuse.** Preserve zccache transaction ordering and retry decisions at the caller; move only primitive imports. |
| `fs::{make_executable,ensure_dir_private,create_dir_all_private,set_readonly,apply_mode,mode}` | Exact KA permission primitives | **Reuse.** Import directly from KA. |
| `fs::make_writable` | Product compatibility treatment for dangling links and Windows missing paths | **Retain product policy** in the artifact/materialization owner. It may call KA `set_readonly`; retain the explicit dangling-link and missing-path cases. |
| `fs::{verbatim_path,from_raw_bytes}` | Exact KA native-call/path-byte conversion | **Reuse.** Direct callers import KA. |
| `fs::{strip_verbatim_prefix,case_fold,from_msys,canonicalize_private_prefix,system_root_candidate}` | Frozen lexical cache-key conventions, not native identity | **Retain product policy** in `zccache-core::path`; use canonical target facts but preserve each spelling transformation byte-for-byte. |
| `fs::{VolumeIdentity,volume_identity,available_space}` | Exact KA volume primitives | **Reuse.** Direct callers import KA resources/filesystem primitives. |
| `host::{os,arch,is_linux,is_macos,is_windows,available_parallelism}` | Exact target and concurrency facts, except legacy names | **Reuse.** Replace with KA imports or small local aliases at the consuming crate. |
| `host::{home_dir,runtime_dir,current_user,cpu_identity_material}` | zccache environment namespace, cache-key labels, `/etc/machine-id`, and PID fallback policy | **Retain product policy** in `zccache-core`; it may use KA target/CPU-feature facts but keeps its established material format. |
| `host::{is_elevated,DefenderError,defender_*,add_defender_exclusion,remove_defender_exclusion}` | Windows elevation query is generic; PowerShell command, parsing, and operator wording are product policy | **Split.** Use KA elevation capability directly; move Defender command/error policy to `zccache-core` or CLI ownership without introducing a native Windows API dependency. |
| `process::{hide_window,configure_process_group}` | Exact KA trampoline/session-leader command configuration | **Reuse.** Consumers call the KA process API directly. |
| `process::{is_alive,executable_path,cpu_ticks}` | Product's best-effort collapse of raw PID observations | **Retain thin product adapters** at the daemon owner if the `false`/`None` collapse is public behavior; their implementation uses KA liveness/image/CPU primitives only. |
| `process::{ExitOutcome,outcome,termination_signal_from_exit_code,NativeExit,crash_label}` | zccache response ABI and diagnostic vocabulary | **Retain product policy** in `zccache-daemon-core`; use KA's native exit conversion as input, preserving the `-(128 + signal)` mapping and Windows distinction. |
| `process::{NativeJobserver,is_supported}` | Exact KA GNU-make jobserver primitive | **Reuse.** Direct callers import KA. |
| `process::{Priority,apply_to_child}` | Exact KA priority type/application bridge | **Reuse** until all caller-owned Tokio child handles are eliminated; then use the semantic KA session/builder priority surface. |
| `process::{detach,redirect_to_log,force,force_group}` | Exact KA standard-stream and raw-PID compatibility operations, except Windows `taskkill /T` tree policy | **Split.** Reuse KA direct operations; retain the Windows tree-command policy in the daemon/CI owner until it has an identity-safe replacement. |
| `process::{sleeping_child,echo_output,attach_owner_death,uses_pre_spawn_owner_death,run_cli_entry}` | Test fixture and host-specific CLI-stack compatibility support | **Move tests** to `zccache-test-support`/test modules. Keep only `run_cli_entry` beside the CLI if the Windows stack reservation remains required; it must not keep a general platform crate alive. |
| `ipc::Endpoint` and its endpoint-string/portable-length/timeout helpers | Product endpoint spelling, test naming, broker conversion, and 30s Unix/5s Windows timeout policy | **Retain compatibility adapter** in `zccache-ipc`. It uses KA endpoint/retirement primitives but preserves raw strings, length rules, conversions, and timeout values. |
| `ipc::{Listener,Stream,PeerIdentity,connect}` | Frozen zccache local transport behavior: Tokio I/O traits, Unix parent hardening/rejection strings, and Windows pool/retry/first-instance lifecycle | **Retain compatibility adapter** in `zccache-ipc`. It wraps opaque KA IPC primitives and owns only product policy/trait adaptation; it must not expose interprocess or named-pipe types. |
| `ipc::{current_user_name,select_host_text,probe_native}` | Endpoint naming and one-shot product diagnostics | **Move to `zccache-ipc`** beside the endpoint adapter. KA retains primitive validation/opening only. |

## Ordered removal plan

1. Move filesystem and executable **reuse** imports directly to kernal-api,
   keeping product path normalization and materialization compatibility helpers
   in their current product crates.
2. Move host and exit-result product policy into `zccache-core` and
   `zccache-daemon-core` respectively. Break the current `zccache-core ->
   zccache-platform` dependency before moving shared host helpers there.
3. Move the complete IPC adapter into `zccache-ipc`, preserving golden frame
   bytes, endpoint selection, peer rejection strings, retry schedules, and the
   single request/response round trip.
4. Move only test fixtures to test support, then remove the
   `legacy-process-test-support` feature.
5. Remove `zccache-platform` from workspace members, workspace dependencies,
   every crate manifest, public re-exports, and the transitional platform
   Dylint baseline. Replace it with a strict kernal-api boundary Dylint.

## Completed source slices (unvalidated)

- `zccache-download-protocol` no longer depends on `zccache-platform`. Its
  download-daemon endpoint selector now retains the exact Unix socket versus
  `\\.\pipe\` spelling locally, using only kernal-api's canonical target
  predicate. The focused source regression covers both target spellings.
- `zccache-core` no longer depends on `zccache-platform`. Its environment-first
  home selection, lexical cache-key path rules, Defender command/error policy,
  and legacy non-Windows elevation rule are now core-owned; private-directory,
  append-log, executable, target, and native elevation primitives directly use
  kernal-api. Existing core path and Defender regressions retain the relevant
  product behavior.
- `zccache-artifact` no longer depends on `zccache-platform`. It directly uses
  canonical link/reparse classification, native-call path preparation, and
  transient-share classification. Its artifact-specific writable policy stays
  local, including the historical dangling-link and Windows-missing-path cases.
- `zccache-compiler` no longer depends on `zccache-platform`. Host target
  predicates and native library naming now come directly from kernal-api;
  zccache's ordered libclang installation search remains compiler-owned and
  has a focused source regression that freezes its product order.
- `zccache-depgraph` no longer depends on `zccache-platform`. It uses
  kernal-api target/path-byte primitives directly and shares zccache's
  cache-key path and native-CPU identity policy from `zccache-core`; focused
  regressions retain the material-label and root-path contracts.
- `zccache-cli-core` no longer depends on `zccache-platform`. Its small
  crate-local compatibility namespace directly re-exports kernal-api
  capability types, delegates Defender support to zccache core, and retains
  the Windows-only CLI stack reservation as CLI product policy.
- `zccache-daemon-core` no longer depends on `zccache-platform`. Its daemon
  adapter retains only zccache's negative-signal response encoding and
  materialization compatibility rule; filesystem, inspection, priority,
  jobserver, stdio, and owner-death containment now delegate to the canonical
  kernal-api/running-process surface.
- `zccache-ipc` no longer depends on `zccache-platform`. Endpoint spelling,
  peer-rejection taxonomy, connect deadlines, and Windows pipe-pool policy
  now live in the IPC owner; its Unix and Windows byte transports use the
  protected kernal-api socket and pipe capabilities directly.
- The published `zccache` facade no longer re-exports the removed internal
  platform crate. Its CI tree-kill fallback remains product-owned, while
  executable discovery, process-group configuration, and target facts call
  kernal-api directly.

## Release gates

Before deleting the crate, source and validation evidence must show:

- no production `zccache_platform` import or manifest dependency;
- no direct production `running-process`, Tokio/Tokio-util, interprocess,
  libc, or windows-sys use for a kernal-api-owned capability;
- unchanged broker frame bytes, payload protocol `0x7A63`, endpoint policy,
  peer authorization, and request round-trip count;
- preserved child containment, cancellation, bounded probe, filesystem
  replacement/identity, and daemon lifecycle behavior on Linux, macOS, and
  Windows; and
- a released, exact public kernal-api pin after the temporary nested
  running-process integration handoff is complete.
