#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
./ffi/scripts/check-native-deps.sh
source_revision="$(git rev-parse HEAD)"
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
  echo 'Build native artifacts from a clean committed source tree.' >&2
  exit 1
fi
: "${ANDROID_NDK_HOME:=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}/ndk/28.2.13676358}"
export ANDROID_NDK_HOME
if [[ ! -d "$ANDROID_NDK_HOME" ]]; then
  echo 'Set ANDROID_NDK_HOME to an installed Android NDK.' >&2
  exit 1
fi
native_alignment_flags='-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384'
if [[ -n "${CARGO_ENCODED_RUSTFLAGS:-}" || ( -n "${RUSTFLAGS:-}" && "$RUSTFLAGS" != "$native_alignment_flags" ) ]]; then
  echo 'Native artifact builds require the reviewed alignment-only RUSTFLAGS.' >&2
  exit 1
fi
export RUSTFLAGS="$native_alignment_flags"
cargo ndk --platform 24 --target armeabi-v7a --target arm64-v8a --target x86_64 \
  --output-dir ffi/generated/android/jniLibs build --release --locked \
  -p playback-gateway-ffi --no-default-features --features torrent --lib
# cargo-ndk copies this dependency's unused cdylib build products. The facade
# links its rlib statically; exported products must contain only the facade.
for abi in armeabi-v7a arm64-v8a x86_64; do
  rm -f "ffi/generated/android/jniLibs/$abi"/libcrc_fast-*.so
done
case "$(uname -s)" in
  Darwin) ndk_host=darwin-x86_64 ;;
  MINGW*|MSYS*|CYGWIN*) ndk_host=windows-x86_64 ;;
  *) ndk_host=linux-x86_64 ;;
esac
readelf_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$ndk_host/bin/llvm-readelf"
for abi in armeabi-v7a arm64-v8a x86_64; do
  library="ffi/generated/android/jniLibs/$abi/libplayback_gateway_ffi.so"
  "$readelf_bin" -lW "$library" | awk '/ LOAD / { if ($NF != "0x4000" && $NF != "0x10000") bad=1; found=1 } END { if (!found || bad) exit 1 }'
  echo "Verified $abi ELF page alignment."
done
if [[ "$(git rev-parse HEAD)" != "$source_revision" || -n "$(git status --porcelain --untracked-files=all)" ]]; then
  echo 'Source changed during native artifact generation.' >&2
  exit 1
fi
NATIVE_BUILD_REVISION="$source_revision" python3 - <<'PY'
import hashlib,json,os,pathlib,subprocess
abis={'armeabi-v7a':'armv7-linux-androideabi','arm64-v8a':'aarch64-linux-android','x86_64':'x86_64-linux-android'}
files={f'ffi/generated/android/jniLibs/{abi}/libplayback_gateway_ffi.so':None for abi in abis}
proof={'version':1,'revision':os.environ['NATIVE_BUILD_REVISION'],'min_sdk':24,'abis':abis,'features':['torrent'],
       'rustflags':os.environ['RUSTFLAGS'],'rustc':subprocess.check_output(['rustc','--version'],text=True).strip(),
       'files':{path:hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest() for path in files}}
pathlib.Path('ffi/generated/android/build.json').write_text(json.dumps(proof,indent=2,sort_keys=True)+'\n')
PY
