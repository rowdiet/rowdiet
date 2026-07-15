#!/bin/sh
# The full local verification matrix — becomes the CI workflow verbatim when the repo ships.
# Soft-skips the wasip1 leg when the wasi-sdk or node are absent (same spirit as adopters'
# soft-skip gates: enforce where present, never block on missing tooling).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
echo "== fmt"
cargo fmt --all --check
echo "== clippy (all features, deny warnings)"
cargo clippy --workspace --all-features -- -D warnings
echo "== tests (all features)"
cargo test --workspace --all-features
echo "== CLI without pg-exact"
cargo check -p rowdiet --no-default-features
echo "== core on wasm32-unknown-unknown (no default features)"
cargo check -p rowdiet-core --no-default-features --target wasm32-unknown-unknown
WASI_SDK="${WASI_SDK:-/opt/wasi-sdk-33.0}"
if [ -d "$WASI_SDK" ] && command -v wasmtime >/dev/null 2>&1; then
    echo "== wasip1 module + wasmtime smoke"
    ./wasm/build-wasip1.sh > /dev/null
    smoke_out=$(printf '{"sources":[{"name":"s.sql","sql":"CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);"}],"fail_over":0}' \
        | wasmtime run target/wasm32-wasip1/wasm/rowdiet-smoke.wasm)
    printf '%s' "$smoke_out" | grep -q '"avoidable_bytes_per_row":8' || { echo "wasm smoke mismatch: $smoke_out"; exit 1; }
    echo "wasm smoke ok"
else
    echo "== wasip1 leg SKIPPED (wasi-sdk or wasmtime missing)"
fi
echo "CI matrix green"
