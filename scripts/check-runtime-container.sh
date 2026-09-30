#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Disposable, isolated real service/process qualification; not deployment.
set -euo pipefail
gateway_image="${1:?Usage: bash scripts/check-runtime-container.sh LOCAL_GATEWAY_IMAGE}"
case "${2:-ts}" in ts) matroska=0 ;; matroska) matroska=1 ;; *) echo 'Fixture mode must be ts or matroska' >&2; exit 2 ;; esac
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
revision=$(git -C "$repo_root" rev-parse HEAD)
image_id=$(docker image inspect "$gateway_image" --format '{{.Id}}')
[[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]]
parent_image="playback-runtime-parent:${image_id#sha256:}"
fixture_image="playback-runtime-fixture:${revision:0:12}-${image_id:7:12}"
docker image tag "$image_id" "$parent_image"
printf 'Source revision: %s\nGateway image: %s\n' "$revision" "$image_id"
git -C "$repo_root" archive "$revision" Cargo.toml Cargo.lock engine control scripts/runtime-fixture.Dockerfile \
  | docker build --pull=false --build-arg "GATEWAY_IMAGE=$parent_image" -f scripts/runtime-fixture.Dockerfile -t "$fixture_image" -
container_id=''
cleanup() {
  if [[ -n "$container_id" ]]; then
    docker container rm --force --volumes "$container_id" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
for test_name in standalone_binary_serves_readiness_locks_storage_and_shuts_down \
  real_service_active_and_stalled_media_shutdown_reaps_workers \
  real_service_crash_stops_media_children_and_restart_cleans_storage; do
  entrypoint=/fixtures/runtime-fixture
  if [[ "$test_name" == standalone_* ]]; then entrypoint=/fixtures/service-fixture; fi
  container_id=$(docker run --detach --pull=never --init --user 0:0 --network none --read-only --no-healthcheck \
    --cap-drop ALL --cap-add NET_ADMIN --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,size=512m --memory 1g --memory-swap 1g --pids-limit 256 --cpus 2 \
    --env "PLAYBACK_TEST_MATROSKA_AV=$matroska" \
    --entrypoint "$entrypoint" "$fixture_image" "$test_name" --ignored --exact --nocapture)
  [[ "$container_id" =~ ^[0-9a-f]{64}$ ]]
  if ! result=$(timeout --signal=TERM --kill-after=5s 120s docker wait "$container_id"); then
    docker logs "$container_id" >&2 || true
    echo 'Runtime fixture exceeded its outer deadline.' >&2; exit 1
  fi
  output=$(docker logs "$container_id" 2>&1)
  printf '%s\n' "$output"
  [[ "$result" == 0 ]]
  grep -Fq "test $test_name ... ok" <<< "$output"
  grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' <<< "$output"
  cleanup; container_id=''
done
echo 'Three named service fixtures ran exactly once; only their disposable containers/data were removed.'
