# wasip1 build recipe (route 3: libpg_query in a Rust-linked wasm module)

Proven end-to-end in the spike (`rowdiet-spike/experiments/ddlpgq-wasi`, runs under wasmtime 47
and Node's V8 WASI; full friction log with verbatim errors in
`rowdiet-spike/research/libpg-query-wasm-verdict.md`). This file is the rowdiet-owned copy of
the working recipe so no future session depends on the spike bundle.

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

## Runtime shape (Phase 2 target)

Reactor-style library module, not a bin: `#[no_mangle] extern "C" fn rowdiet_lint(ptr, len) ->
ptr` with JSON in/out plus exported alloc/free (go-pgquery precedent). Browser loads it via
`browser_wasi_shim`; a ~100-line hand-rolled JS loader replaces wasm-bindgen (which refuses this
target — that is the structural fact that forced route 3 in the first place).
