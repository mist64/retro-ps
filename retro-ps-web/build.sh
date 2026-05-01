#!/usr/bin/env bash
# Rebuild the wasm package consumed by the static frontend.
# Outputs to ./pkg/ (gitignored — regenerated each run).
set -euo pipefail
cd "$(dirname "$0")/../retro-ps-wasm"
wasm-pack build --target web --release --out-dir ../retro-ps-web/pkg "$@"
