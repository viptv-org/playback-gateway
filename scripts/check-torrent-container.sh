#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Isolated torrent/archive qualification. Builds local fixtures; no deployment.
set -euo pipefail
image="${1:?Usage: bash scripts/check-torrent-container.sh LOCAL_GATEWAY_IMAGE [all|torrent|rolling-torrent|stored-rar|compressed-rar|bounded-archive|egress]}"
mode="${2:-all}"
if [[ "$mode" == all ]]; then
  for part in torrent rolling-torrent stored-rar compressed-rar bounded-archive egress; do "$0" "$image" "$part"; done
  exit 0
fi
case "$mode" in
  torrent) test_name=workers::torrent_tests::real_torrent_viewers_share_output_seek_and_reap_owned_input ;;
  rolling-torrent) test_name=workers::torrent_tests::rolling_torrent_viewers_decode_seek_with_bounded_input ;;
  stored-rar) test_name=workers::archive_tests::real_stored_multivolume_torrent_archive_hls_shares_and_seeks ;;
  compressed-rar) test_name=workers::archive_tests::real_compressed_torrent_archive_hls_extracts_before_playback ;;
  egress) test_name=workers::security_tests::real_torrent_disguised_playlist_cannot_contact_private_child ;;
  bounded-archive) test_name=workers::archive_tests::real_archive_spool_kernel_enospc_is_processing_error_and_removes_partial_output ;;
  *) echo 'Fixture mode must be all, torrent, rolling-torrent, stored-rar, compressed-rar, bounded-archive or egress' >&2; exit 2 ;;
esac
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
revision=$(git -C "$root" rev-parse HEAD)
digest=$(docker image inspect "$image" --format '{{.Id}}')
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
parent="playback-torrent-parent:${digest#sha256:}"
fixture="playback-torrent-fixture:${revision:0:12}-${digest:7:12}"
build_network="${PLAYBACK_TEST_BUILD_NETWORK:-default}"
docker image tag "$digest" "$parent"
printf 'Source revision: %s\nGateway image: %s\nMode: %s\n' "$revision" "$digest" "$mode"
git -C "$root" archive "$revision" Cargo.toml Cargo.lock engine control ffi torrent-policy third_party/rqbit scripts/torrent-fixture.Dockerfile \
  | docker build --network "$build_network" --pull=false --build-arg "GATEWAY_IMAGE=$parent" \
      -f scripts/torrent-fixture.Dockerfile -t "$fixture" -
id=''
trap 'if [[ -n "$id" ]]; then docker rm --force --volumes "$id" >/dev/null 2>&1 || true; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
args=(--detach --pull=never --init --user 10001:10001 --network none --read-only --no-healthcheck
  --cap-drop ALL --security-opt no-new-privileges --memory 2g --memory-swap 2g --pids-limit 256 --cpus 2
  --tmpfs /tmp:rw,nosuid,nodev,size=512m,mode=1777)
if [[ "$mode" == bounded-archive ]]; then
  args+=(--tmpfs /var/lib/playback-gateway/archive-cache:rw,noexec,nosuid,nodev,size=2m,uid=10001,gid=10001,mode=0700
    --env PLAYBACK_TEST_ARCHIVE_CACHE=/var/lib/playback-gateway/archive-cache)
fi
id=$(docker run "${args[@]}" "$fixture" "$test_name" --ignored --exact --nocapture --test-threads=1)
[[ "$id" =~ ^[0-9a-f]{64}$ ]]
if ! result=$(timeout --signal=TERM --kill-after=5s 300s docker wait "$id"); then
  docker logs "$id" >&2 || true
  echo 'Torrent/archive fixture exceeded its outer deadline.' >&2; exit 1
fi
output=$(docker logs "$id" 2>&1); printf '%s\n' "$output"
[[ "$result" == 0 ]]
grep -Fq "test $test_name ..." <<< "$output"
grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' <<< "$output"
printf 'Torrent/archive %s fixture ran exactly once. image=%s revision=%s\n' "$mode" "$digest" "$revision"
if [[ "$mode" == bounded-archive ]]; then
  echo 'Bounded qualification covers compressed archive preparation ENOSPC, partial cleanup and recovery; it does not certify failure of a previously ready viewer.'
fi
