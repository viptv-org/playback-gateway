#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# No production service/data. Private account/catalog boundary, real gateway media.
set -euo pipefail
image="${1:?Usage: bash scripts/check-torrent-browser.sh IMAGE TV_WEB_ROOT LOCAL_CERT_DIR [TLS_PORT]}"
tv_root=$(realpath "${2:?Built TV-web checkout required}")
cert_dir=$(realpath "${3:?Trusted local HTTPS certificate directory required}")
port="${4:-8444}"
[[ "$port" =~ ^[0-9]+$ ]] && ((port>=1024 && port<=65535))
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
[[ -f "$tv_root/dist/index.html" && -f "$tv_root/tests/e2e/helpers/responsiveBackend.ts" ]]
[[ -f "$cert_dir/viptv.local.test.crt" && -f "$cert_dir/viptv.local.test.key" ]]
[[ $(node -p 'Number(process.versions.node.split(".")[0])') -ge 24 ]]
q=$(mktemp -d "$root/target/qualification/browser-XXXXXX")
chmod 700 "$q"
export QUALIFICATION_DIR="$q" TV_WEB_ROOT="$tv_root"
export VIPTV_TEST_BROWSER_ORIGIN="https://viptv.local.test:$port"
export VIPTV_TEST_API_ORIGIN="$VIPTV_TEST_BROWSER_ORIGIN"
socket_dir=$(mktemp -d /tmp/pg-browser-XXXXXX)
ln -s "$q/gateway.sock" "$socket_dir/gateway.sock"
export GATEWAY_SOCKET="$socket_dir/gateway.sock"
id='' caddy_pid=''
cleanup() {
 [[ -z "$caddy_pid" ]] || kill "$caddy_pid" 2>/dev/null || true
 [[ -z "$id" ]] || docker logs "$id" > "$q/fixture.log" 2>&1 || true
 [[ -z "$id" ]] || docker rm --force --volumes "$id" >/dev/null 2>&1 || true
 python3 -c 'from pathlib import Path; import sys; p=Path(sys.argv[1]); (p/"gateway.sock").unlink(missing_ok=True); p.rmdir()' "$socket_dir"
}
trap cleanup EXIT
CARGO_BUILD_JOBS=2 cargo test --locked -p playback-gateway --test torrent_browser --no-run --message-format=json > "$q/compile.json" 2> "$q/compile.log"
binary=$(python3 -c 'import json,sys; rows=[json.loads(line) for line in open(sys.argv[1])]; bins=[r["executable"] for r in rows if r.get("reason")=="compiler-artifact" and r.get("target",{}).get("name")=="torrent_browser" and r.get("executable")]; assert len(bins)==1; print(bins[0])' "$q/compile.json")
args=(--detach --pull=never --init --network none --read-only --no-healthcheck
 --cap-drop ALL --cap-add NET_ADMIN --cap-add CHOWN --cap-add SETUID --cap-add SETGID --cap-add DAC_OVERRIDE --cap-add KILL
 --security-opt no-new-privileges --memory 1g --memory-swap 1g --pids-limit 256 --cpus 2
 --tmpfs /tmp:rw,nosuid,nodev,size=512m,mode=1777 --user 0:0
 --env PLAYBACK_TEST_ISOLATED_NETWORK=container --env PLAYBACK_TEST_SERVICE_BINARY=/usr/local/bin/playback-gateway
 --env PLAYBACK_TEST_FFMPEG=/opt/ffmpeg/bin/ffmpeg --env PLAYBACK_TEST_FFPROBE=/opt/ffmpeg/bin/ffprobe
 --mount "type=bind,source=$binary,target=/fixtures/browser-fixture,readonly"
 --mount "type=bind,source=$q,target=/qualification"
 --entrypoint /fixtures/browser-fixture "$image" standalone_torrent_browser_uid10001 --ignored --exact --nocapture --test-threads=1)
id=$(docker run "${args[@]}")
for _ in {1..200}; do [[ ! -f "$q/fixture.json" ]] || break; sleep .1; done
[[ -f "$q/fixture.json" ]]
cat > "$q/Caddyfile" <<CADDY
{
 admin off
 auto_https disable_redirects
 log default {
  output discard
 }
}
$VIPTV_TEST_BROWSER_ORIGIN {
 tls $cert_dir/viptv.local.test.crt $cert_dir/viptv.local.test.key
 handle /media/* {
  reverse_proxy unix/$GATEWAY_SOCKET
 }
 handle /tv/* {
  uri strip_prefix /tv
  root * $tv_root/dist
  try_files {path} /index.html
  file_server
 }
}
CADDY
caddy run --config "$q/Caddyfile" > "$q/caddy.log" 2>&1 & caddy_pid=$!
for _ in {1..50}; do if curl -sS -o /dev/null "$VIPTV_TEST_BROWSER_ORIGIN/tv/" 2>/dev/null; then break; fi; sleep .1; done
[[ $(curl -sS -o /dev/null -w '%{http_code} %{ssl_verify_result}' "$VIPTV_TEST_BROWSER_ORIGIN/tv/") == '200 0' ]]
timeout --signal=TERM --kill-after=5s 180s node "$root/scripts/check-torrent-browser.mjs" > "$q/browser.log" 2>&1
result=$(timeout --signal=TERM --kill-after=5s 120s docker wait "$id")
docker logs "$id" > "$q/fixture.log" 2>&1
cat "$q/browser.log" "$q/fixture.log"
[[ "$result" == 0 ]]
grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$q/fixture.log"
printf 'Trusted HTTPS browser fixture passed. Private artifacts: %s\n' "$q"
