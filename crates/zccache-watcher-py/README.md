# zccache-watcher-py

The PyO3 extension module `zccache.watcher._native`. Thin bindings over
[`zccache-watcher`](../zccache-watcher); all watcher logic lives there.

## Why this crate exists (zccache#1497)

`zccache-watcher` used to declare `crate-type = ["rlib", "cdylib"]` and gate its
PyO3 code behind a `python` feature. Cargo has no way to make a crate-type
conditional, so **every** build that needed the rlib also linked the cdylib —
including the Windows release binaries, which depend on `zccache-watcher`
through the umbrella `zccache` crate.

That is benign until `-C target-feature=+crt-static` enters the picture. The
shipped `zccache.exe` needs a static CRT (zccache#269 — without it the loader
rejects the image with `STATUS_DLL_INIT_FAILED` on hosts missing or
version-skewing `VCRUNTIME140.dll`), but `RUSTFLAGS` is process-global, so the
flag reached the unwanted cdylib too:

```
lld-link: error: duplicate symbol: __vcrt_InitializeCriticalSectionEx
  >>> libvcruntime.lib(winapi_downlevel.obj)   (static CRT)
  >>> vcruntime.lib(VCRUNTIME140.dll)          (dynamic CRT)
```

`winapi_downlevel.obj` carries the Windows 7-era API shims, which exist only in
the **x64** static CRT — aarch64 never supported those OSes, which is why the
ARM64 lane linked the same cdylib with the same flag and passed.

Splitting the cdylib into its own crate fixes it at the root: nothing that wants
the watcher *library* pulls in a dynamic-CRT artifact any more. A PyO3 extension
must share the host interpreter's dynamic CRT, so this crate is built by the
Python-extension step, which deliberately runs without `+crt-static`.

`[lib] name = "zccache_watcher"` is deliberate — it keeps the output filename
(`zccache_watcher.dll` / `.so` / `.dylib`) unchanged for the packaging step.
