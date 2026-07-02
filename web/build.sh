#!/bin/sh
# Build the wasip1 module and stage it for the page. Serve web/ statically afterwards, e.g.:
#   python3 -m http.server -d web 8017
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
"$ROOT/wasm/build-wasip1.sh"
cp "$ROOT/target/wasm32-wasip1/wasm/rowdiet_wasm.wasm" "$ROOT/web/rowdiet.wasm"
ls -l "$ROOT/web/rowdiet.wasm"
