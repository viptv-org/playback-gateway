#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_BUILD_JOBS=2
cargo build --locked -p playback-gateway-ffi --no-default-features --features torrent --lib
cargo run --locked -p playback-gateway-ffi --features bindgen --bin playback-uniffi-bindgen -- generate \
  --library target/debug/libplayback_gateway_ffi.so --language kotlin --no-format --out-dir ffi/generated/kotlin
find ffi/generated/kotlin -name '*.kt' -exec sed -i 's/[[:space:]]\+$//' {} +
# UniFFI leaves blank lines at EOF; retain interior spacing and one final newline.
find ffi/generated/kotlin -name '*.kt' -exec sed -i ':a;/^\n*$/{$d;N;ba;}' {} +
