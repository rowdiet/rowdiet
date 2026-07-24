#!/bin/sh
# Build rowdiet-wasm (reactor cdylib + smoke bin) for wasm32-wasip1 with the pinned wasi-sdk
# recipe — wasm/README.md documents each flag. WASI_SDK overrides the default path.
set -eu
WASI_SDK="${WASI_SDK:-/opt/wasi-sdk-33.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export CC_wasm32_wasip1="$WASI_SDK/bin/clang"
export AR_wasm32_wasip1="$WASI_SDK/bin/llvm-ar"
export CFLAGS_wasm32_wasip1="--sysroot=$WASI_SDK/share/wasi-sysroot -isystem $ROOT/wasm/stub-include \
 -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID \
 -DEHOSTDOWN=200 -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
export BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1="--sysroot=$WASI_SDK/share/wasi-sysroot \
 -isystem $ROOT/wasm/stub-include -fvisibility=default"
export RUSTFLAGS="-C link-arg=-L$WASI_SDK/share/wasi-sysroot/lib/wasm32-wasip1 -C link-arg=-lsetjmp \
 -C link-arg=-lwasi-emulated-signal -C link-arg=-lwasi-emulated-process-clocks \
 -C link-arg=-lwasi-emulated-mman -C link-arg=-lwasi-emulated-getpid"
cd "$ROOT"
cargo build --profile wasm --target wasm32-wasip1 -p rowdiet-wasm
# Optional size pass (brew install binaryen). The feature flags matter: the module carries real
# wasm-EH (exnref) from the sjlj lowering.
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-exception-handling --enable-bulk-memory --enable-nontrapping-float-to-int \
    --enable-sign-ext --enable-mutable-globals \
    "$ROOT/target/wasm32-wasip1/wasm/rowdiet_wasm.wasm" -o "$ROOT/target/wasm32-wasip1/wasm/rowdiet_wasm.wasm.opt"
  mv "$ROOT/target/wasm32-wasip1/wasm/rowdiet_wasm.wasm.opt" "$ROOT/target/wasm32-wasip1/wasm/rowdiet_wasm.wasm"
else
  echo "wasm-opt not found — skipping size pass"
fi
ls -l "$ROOT"/target/wasm32-wasip1/wasm/*.wasm
