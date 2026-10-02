#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Actual service/API qualification from committed fixtures; no deployment.
set -euo pipefail
image="${1:?Usage: bash scripts/check-torrent-api-container.sh LOCAL_IMAGE}"
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
revision=$(git -C "$root" rev-parse HEAD)
digest=$(docker image inspect "$image" --format '{{.Id}}')
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
parent="playback-torrent-api-parent:${digest#sha256:}"
fixture="playback-torrent-api-fixture:${revision:0:12}-${digest:7:12}"
docker image tag "$digest" "$parent"
printf 'Source revision: %s\nGateway image: %s\n' "$revision" "$digest"
git -C "$root" archive "$revision" Cargo.toml Cargo.lock engine control ffi scripts/runtime-fixture.Dockerfile \
  | docker build --network "${PLAYBACK_TEST_BUILD_NETWORK:-default}" --pull=false \
      --build-arg "GATEWAY_IMAGE=$parent" -f scripts/runtime-fixture.Dockerfile -t "$fixture" -
id=''
trap 'if [[ -n "$id" ]]; then docker rm --force --volumes "$id" >/dev/null 2>&1 || true; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
for test_name in standalone_torrent_runtime_off_refuses_before_source_io standalone_enabled_torrent_public_api_shares_seeks_and_reclaims; do
  args=(--detach --pull=never --init --network none --read-only --no-healthcheck
    --cap-drop ALL --security-opt no-new-privileges --memory 1g --memory-swap 1g
    --pids-limit 256 --cpus 2 --tmpfs /tmp:rw,nosuid,nodev,size=512m,mode=1777)
  if [[ "$test_name" == standalone_torrent_runtime_off_refuses_before_source_io ]]; then
    args+=(--user 10001:10001 --entrypoint /fixtures/service-fixture)
  else
    # Only the isolated fixture driver needs NET_ADMIN for its public-classified
    # loopback alias. This mode does not qualify the production UID envelope.
    args+=(--user 0:0 --cap-add NET_ADMIN --entrypoint /fixtures/torrent_runtime-fixture)
  fi
  id=$(docker run "${args[@]}" "$fixture" "$test_name" --ignored --exact --nocapture --test-threads=1)
  [[ "$id" =~ ^[0-9a-f]{64}$ ]]
  if ! result=$(timeout --signal=TERM --kill-after=5s 90s docker wait "$id"); then
    docker logs "$id" >&2 || true
    echo 'Torrent API fixture exceeded its outer deadline.' >&2; exit 1
  fi
  output=$(docker logs "$id" 2>&1); printf '%s\n' "$output"
  [[ "$result" == 0 ]]
  grep -Fq "test $test_name ..." <<< "$output"
  grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' <<< "$output"
  docker rm --force --volumes "$id" >/dev/null; id=''
done
printf 'Both actual service/API fixtures ran exactly once. image=%s revision=%s\n' "$digest" "$revision"
