#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_BUILD_JOBS=2
: "${ANDROID_NDK_HOME:=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}/ndk/28.2.13676358}"
export ANDROID_NDK_HOME
if [[ ! -d "$ANDROID_NDK_HOME" ]]; then
  echo 'Set ANDROID_NDK_HOME to an installed Android NDK.' >&2
  exit 1
fi
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384"
cargo ndk --platform 23 --target armeabi-v7a --target arm64-v8a --target x86 --target x86_64 \
  --output-dir ffi/generated/android/jniLibs build --release --locked \
  -p playback-gateway-ffi --no-default-features --features torrent --lib
# cargo-ndk also copies this dependency's unused cdylib build products. The
# facade links its rlib statically and has only Android system DT_NEEDED entries.
for abi in armeabi-v7a arm64-v8a x86 x86_64; do
  rm -f "ffi/generated/android/jniLibs/$abi"/libcrc_fast-*.so
done
# ELF LOAD segment alignment is verified independently for every produced ABI.
readelf_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
for abi in armeabi-v7a arm64-v8a x86 x86_64; do
  library="ffi/generated/android/jniLibs/$abi/libplayback_gateway_ffi.so"
  "$readelf_bin" -lW "$library" | awk '/ LOAD / { if ($NF != "0x4000" && $NF != "0x10000") exit 1; found=1 } END { if (!found) exit 1 }'
  echo "Verified $abi ELF page alignment."
done
