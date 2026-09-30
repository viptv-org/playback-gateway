#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
set -euo pipefail
image="${1:?Usage: bash scripts/check-container-envelope.sh LOCAL_GATEWAY_IMAGE}"
image_id=$(docker image inspect "$image" --format '{{.Id}}')
[[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]]
fixture_suffix=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
fixture_volume="pgw-qualified-data-$fixture_suffix"
fixture_bootstrap=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
archive_dir=$(mktemp -d /tmp/pgw-qualified-volume.XXXXXX)
container_id=''
archive_ok=false
cleanup() {
  if [[ -n "$container_id" ]]; then docker rm --force --volumes "$container_id" >/dev/null 2>&1 || true; fi
  if [[ "$archive_ok" == true ]]; then
    docker volume rm "$fixture_volume" >/dev/null
    printf 'Removed only fixture container/volume; stopped data is recoverable from %s/fixture-data.tar\n' "$archive_dir"
  else
    printf 'Retained disposable fixture volume %s for inspection.\n' "$fixture_volume" >&2
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker volume create "$fixture_volume" >/dev/null
container_id=$(API_KEY="$fixture_bootstrap" docker run --detach --pull=never --network none --read-only \
  --cap-drop ALL --security-opt no-new-privileges --tmpfs /tmp:rw,nosuid,nodev,size=64m \
  --pids-limit 1024 --env API_KEY --mount "type=volume,source=$fixture_volume,target=/var/lib/playback-gateway" "$image_id")
[[ "$container_id" =~ ^[0-9a-f]{64}$ ]]
ready() {
  for attempt in $(seq 1 30); do
    if docker exec "$container_id" curl --fail --silent --max-time 2 http://127.0.0.1:8090/ready >/dev/null 2>&1; then return; fi
    sleep 1
  done
  echo 'Container readiness deadline exceeded.' >&2; return 1
}
ready
envelope=$(docker inspect "$container_id" --format '{{.Config.User}}|{{.HostConfig.ReadonlyRootfs}}|{{json .HostConfig.CapDrop}}')
[[ "$envelope" == '10001:10001|true|["ALL"]' ]]
docker exec "$container_id" sh -c 'test "$(id -u)" = 10001 && test "$(id -g)" = 10001 && test "$(stat -c %a /var/lib/playback-gateway)" = 700 && grep -q "^NoNewPrivs:[[:space:]]*1$" /proc/self/status && grep -q "^CapEff:[[:space:]]*0000000000000000$" /proc/self/status && ! touch /readonly-root-test 2>/dev/null'
for attempt in $(seq 1 30); do
  if [[ $(docker inspect "$container_id" --format '{{.State.Health.Status}}') == healthy ]]; then break; fi
  sleep 1
done
[[ $(docker inspect "$container_id" --format '{{.State.Health.Status}}') == healthy ]]
if duplicate=$(API_KEY="$fixture_bootstrap" docker run --rm --pull=never --network none --read-only --cap-drop ALL --security-opt no-new-privileges \
  --env API_KEY --mount "type=volume,source=$fixture_volume,target=/var/lib/playback-gateway" "$image_id" 2>&1); then
  echo 'A duplicate volume owner was incorrectly accepted.' >&2; exit 1
fi
grep -Fq 'data directory already in use' <<< "$duplicate"
docker stop --time 10 "$container_id" >/dev/null
[[ $(docker inspect "$container_id" --format '{{.State.ExitCode}}') == 0 ]]
docker start "$container_id" >/dev/null
ready
docker stop --time 10 "$container_id" >/dev/null
[[ $(docker inspect "$container_id" --format '{{.State.ExitCode}}') == 0 ]]
# Archive only stopped fixture data. The individual writable file bind avoids
# opening the private host directory to the container's different non-root UID.
touch "$archive_dir/fixture-data.tar"; chmod 666 "$archive_dir/fixture-data.tar"
docker run --rm --pull=never --network none --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user 10001:10001 --entrypoint tar \
  --mount "type=volume,source=$fixture_volume,target=/data,readonly" \
  --mount "type=bind,source=$archive_dir/fixture-data.tar,target=/snapshot.tar" \
  "$image_id" -cf /snapshot.tar -C /data .
chmod 600 "$archive_dir/fixture-data.tar"
archive_ok=true
printf 'Image %s: non-root/read-only/dropped-capabilities/readiness/private-volume/lock/restart/SIGTERM checks passed.\n' "$image_id"
