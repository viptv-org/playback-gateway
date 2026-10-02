#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Disposable, isolated progressive live delivery qualification; not deployment.
set -euo pipefail
image="${1:?Usage: bash scripts/check-progressive-container.sh LOCAL_GATEWAY_IMAGE [ts|matroska|ts-tls|matroska-tls|bounded|bounded-matroska]}"
mode="${2:-ts}"
# Override only build networking on hosts whose kernel cannot create veth pairs.
# Qualification containers still use --network none.
build_network="${PLAYBACK_TEST_BUILD_NETWORK:-default}"
matroska=0; tls=0; bounded=0
case "$mode" in
  ts) ;; matroska) matroska=1 ;; ts-tls) tls=1 ;; matroska-tls) matroska=1; tls=1 ;;
  bounded) bounded=1 ;; bounded-matroska) bounded=1; matroska=1 ;;
  *) echo 'Fixture mode must be ts, matroska, ts-tls, matroska-tls, bounded or bounded-matroska' >&2; exit 2 ;;
esac
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
revision=$(git -C "$root" rev-parse HEAD)
digest=$(docker image inspect "$image" --format '{{.Id}}')
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
parent="playback-progressive-parent:${digest#sha256:}"
fixture="playback-progressive-fixture:${revision:0:12}-${digest:7:12}"
docker image tag "$digest" "$parent"
printf 'Source revision: %s\nGateway image: %s\nMode: %s\n' "$revision" "$digest" "$mode"
git -C "$root" archive "$revision" Cargo.toml Cargo.lock engine control ffi scripts/runtime-fixture.Dockerfile \
 | docker build --network "$build_network" --pull=false --build-arg "GATEWAY_IMAGE=$parent" -f scripts/runtime-fixture.Dockerfile -t "$fixture" -
if [[ "$tls" == 1 ]]; then
  # The same fixture-only CA/leaf and TLS proxy as the subtitle HTTPS fixture.
  git -C "$root" archive "$revision" scripts/subtitle-fixture.Dockerfile \
   | docker build --network "$build_network" --pull=false --build-arg "FIXTURE_IMAGE=$fixture" -f scripts/subtitle-fixture.Dockerfile -t "$fixture-tls" -
  fixture="$fixture-tls"
fi
id=''
trap 'if [[ -n "$id" ]]; then docker rm --force --volumes "$id" >/dev/null 2>&1 || true; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
args=(--detach --pull=never --init --user 0:0 --network none --read-only --no-healthcheck
  --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges
  --memory 1g --memory-swap 1g --pids-limit 256 --cpus 2
  --env PLAYBACK_TEST_PROGRESSIVE=1 --env "PLAYBACK_TEST_MATROSKA_AV=$matroska"
  --env "PLAYBACK_TEST_SUBTITLE_TLS=$tls")
if [[ "$bounded" == 1 ]]; then
  test_name=real_progressive_kernel_bounded_media_full_fails_and_reclaims
  args+=(--cap-add DAC_OVERRIDE --cap-add SETUID --cap-add SETGID --cap-add KILL
    --tmpfs /tmp:rw,nosuid,nodev,size=128m
    --tmpfs /var/lib/playback-gateway/media:rw,size=2m,uid=10001,gid=10001,mode=0700,noexec,nosuid,nodev
    --env PLAYBACK_TEST_DATA_DIR=/var/lib/playback-gateway)
else
  test_name=real_progressive_image_share_late_join_evict_isolate_and_reap
  args+=(--tmpfs /tmp:rw,nosuid,nodev,size=512m)
fi
id=$(docker run "${args[@]}" "$fixture" "$test_name" --ignored --exact --nocapture)
[[ "$id" =~ ^[0-9a-f]{64}$ ]]
if ! result=$(timeout --signal=TERM --kill-after=5s 300s docker wait "$id"); then
  docker logs "$id" >&2 || true
  echo 'Progressive fixture exceeded its outer deadline.' >&2; exit 1
fi
output=$(docker logs "$id" 2>&1); printf '%s\n' "$output"
[[ "$result" == 0 ]]
grep -Fq "test $test_name ... ok" <<< "$output"
grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' <<< "$output"
printf 'Progressive %s fixture ran exactly once. image=%s revision=%s\n' "$mode" "$digest" "$revision"
