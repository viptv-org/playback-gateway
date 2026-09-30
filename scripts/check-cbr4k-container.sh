#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
set -euo pipefail
image="${1:?Usage: bash scripts/check-cbr4k-container.sh LOCAL_GATEWAY_IMAGE}"
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
revision=$(git -C "$root" rev-parse HEAD)
digest=$(docker image inspect "$image" --format '{{.Id}}')
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
parent="playback-cbr4k-parent:${digest#sha256:}"
fixture="playback-cbr4k-fixture:${revision:0:12}-${digest:7:12}"
docker image tag "$digest" "$parent"
git -C "$root" archive "$revision" Cargo.toml Cargo.lock engine control scripts/runtime-fixture.Dockerfile \
 | docker build --pull=false --build-arg "GATEWAY_IMAGE=$parent" -f scripts/runtime-fixture.Dockerfile -t "$fixture" -
id=''
trap 'if [[ -n "$id" ]]; then docker rm --force --volumes "$id" >/dev/null 2>&1 || true; fi' EXIT
id=$(docker run --detach --pull=never --init --user 0:0 --network none --read-only --no-healthcheck \
 --cap-drop ALL --cap-add NET_ADMIN --cap-add DAC_OVERRIDE --cap-add SETUID --cap-add SETGID --cap-add KILL --security-opt no-new-privileges \
 --tmpfs /tmp:rw,nosuid,nodev,size=256m \
 --tmpfs /var/lib/playback-gateway/media:rw,size=1g,uid=10001,gid=10001,mode=0700,noexec,nosuid,nodev \
 --env PLAYBACK_TEST_DATA_DIR=/var/lib/playback-gateway --env PLAYBACK_TEST_CBR_4K=1 \
 --env LIVE_REPLAY_MAX_BYTES=536870912 \
 --memory 2g --memory-swap 2g --pids-limit 256 --cpus 2 \
 "$fixture" real_cbr_4k_copy_retains_over_old_budget_and_reclaims_spool --ignored --exact --nocapture)
[[ "$id" =~ ^[0-9a-f]{64}$ ]]
result=$(timeout --signal=TERM --kill-after=5s 120s docker wait "$id")
output=$(docker logs "$id" 2>&1);printf '%s\n' "$output"
[[ "$result" == 0 ]]
grep -Fq 'test real_cbr_4k_copy_retains_over_old_budget_and_reclaims_spool ... ok' <<< "$output"
grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' <<< "$output"
printf 'Committed CBR4K copy/reload/retention/reap fixture ran once. image=%s revision=%s\n' "$digest" "$revision"
