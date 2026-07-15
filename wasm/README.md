# wasip1 build recipe (route 3: libpg_query in a Rust-linked wasm module)

Proven end-to-end: the module built with this recipe runs under wasmtime 47 and Node's WASI,
including PostgreSQL's sigsetjmp error path. Every flag below earned its place against a real
build failure.

## Pins

- `wasi-sdk` **33.0** (tarball install; arm64-macos proven — pin per-platform tarballs in CI)
- `pg_query` **= 6.1.1** (PG17 grammar; version-locked to the PG major we claim)
- Rust **stable** (1.96 proven), target `wasm32-wasip1` (`rustup target add wasm32-wasip1`)
- wasmtime 47 for native smoke runs; `browser_wasi_shim` for the page (pin at Phase 2)

## Stub headers (`stub-include/`)

Three tiny, self-documenting headers travel with the build — WASI preview1 lacks them and
libpg_query only needs declarations (or, for `setjmp.h`, a name mapping):

- `netdb.h` — declarations only; libpg_query never resolves names.
- `syslog.h` — macro-discarded; the syslog destination is never enabled.
- `setjmp.h` — maps `sigsetjmp(env, 0)` → `setjmp(env)`; **semantically exact** on WASI (no
  signal masks exist, and PostgreSQL always passes `savemask = 0`). LLVM's wasm SJLJ lowering
  only recognizes the `setjmp`/`longjmp` names.

## The build

`./wasm/build-wasip1.sh` runs everything below against the pinned SDK (override with `WASI_SDK`)
using the size-tuned `wasm` cargo profile; smoke-run the result with
`echo '{"sources":[...]}' | wasmtime run target/wasm32-wasip1/wasm/rowdiet-smoke.wasm`.
The reactor cdylib (`rowdiet_wasm.wasm`) must be instantiated through the WASI shim's
`initialize()` path — never `start()` — with `_initialize` called before any `rowdiet_*` export.

```sh
WASI_SDK=/path/to/wasi-sdk-33.0
CC_wasm32_wasip1="$WASI_SDK/bin/clang" \
AR_wasm32_wasip1="$WASI_SDK/bin/llvm-ar" \
CFLAGS_wasm32_wasip1="--sysroot=$WASI_SDK/share/wasi-sysroot -isystem $(pwd)/wasm/stub-include \
  -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_MMAN \
  -D_WASI_EMULATED_GETPID -DEHOSTDOWN=200 \
  -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false" \
BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1="--sysroot=$WASI_SDK/share/wasi-sysroot \
  -isystem $(pwd)/wasm/stub-include -fvisibility=default" \
RUSTFLAGS="-C link-arg=-L$WASI_SDK/share/wasi-sysroot/lib/wasm32-wasip1 -C link-arg=-lsetjmp \
  -C link-arg=-lwasi-emulated-signal -C link-arg=-lwasi-emulated-process-clocks \
  -C link-arg=-lwasi-emulated-mman -C link-arg=-lwasi-emulated-getpid" \
  cargo build --release --target wasm32-wasip1 -p rowdiet-wasm --features pg-exact
```

Gotchas, learned the hard way:

- `pg_query`'s build.rs does **not** rerun on env-var changes — after editing flags, delete
  `target/wasm32-wasip1/release/{build,.fingerprint}/pg_query-*`.
- `-fvisibility=default` is required for bindgen to see the symbols.
- EH encoding is **exnref** (`-wasm-use-legacy-eh=false`): all major browsers since 2025 and
  current wasmtime/V8. For an older browser floor, rebuild with legacy EH.
- Measured size ballpark: ~2.2 MB raw / ~390 KB gzip.

## Browser loader contract (pins + verified gotchas, 2026-07-23)

- Pin `@bjorn3/browser_wasi_shim@0.4.2` (npm latest since 2025-06-22; no 2026 release exists).
- Instantiate the reactor with `wasi.initialize(instance)`, never `wasi.start()` (throws on a
  reactor). The shim's README documents only the start() flow — do not follow it here.
  `initialize()` is mandatory even without an `_initialize` export (it records the instance every
  syscall dereferences); our wasi-sdk link DOES export `_initialize`, which it calls if present.
- `rowdiet_lint` returns a packed u64 — JS: `Number(v >> 32n)` / `Number(v & 0xffffffffn)`.
- `memory.grow` invalidates views: build a fresh `Uint8Array(memory.buffer)` AFTER each
  `rowdiet_lint` call, before slicing the output. A cached view is the classic loader bug here.
- Wrap export calls in try/catch: aborts surface as the shim's `WASIProcExit` exception
  (`initialize()` and direct calls do not catch it; only `start()` would).
- go-pgquery is ABI-*shape* precedent only: its shipped module targets wazero/wasix (host-side
  setjmp snapshots, no Wasm-EH) and is not browser-runnable. This repo's wasi-sdk-33 + wasm-EH
  route is the browser-correct one.
- libpg_query init: no explicit init export needed single-threaded — lazy init inside the parse
  call covered the wasmtime smoke run; revisit only for exotic multi-instance hosts.

## Runtime shape (Phase 2 target)

Reactor-style library module, not a bin: `#[no_mangle] extern "C" fn rowdiet_lint(ptr, len) ->
ptr` with JSON in/out plus exported alloc/free (go-pgquery precedent). Browser loads it via
`browser_wasi_shim`; a ~100-line hand-rolled JS loader replaces wasm-bindgen (which refuses this
target — that is the structural fact that forced route 3 in the first place).
