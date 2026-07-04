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
WASI_SDK="${WASI_SDK:-$HOME/projs/rowdiet-spike/experiments/wasi-sdk-33.0-arm64-macos}"
if [ -d "$WASI_SDK" ] && command -v node >/dev/null 2>&1; then
    echo "== wasip1 module + loader smoke"
    ./web/build.sh
    node web/smoke.mjs
else
    echo "== wasip1 leg SKIPPED (wasi-sdk or node missing)"
fi
echo "CI matrix green"
