#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
./ffi/scripts/check-native-deps.sh
cargo build --locked -p playback-gateway-ffi --no-default-features --features torrent --lib
target_dir="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
cargo run --locked -p playback-gateway-ffi --features bindgen --bin playback-uniffi-bindgen -- generate \
  --library "$target_dir/debug/libplayback_gateway_ffi.so" --language kotlin --no-format --out-dir ffi/generated/kotlin
find ffi/generated/kotlin -name '*.kt' -exec sed -i 's/[[:space:]]\+$//' {} +
# UniFFI leaves blank lines at EOF; retain interior spacing and one final newline.
find ffi/generated/kotlin -name '*.kt' -exec sed -i ':a;/^\n*$/{$d;N;ba;}' {} +
