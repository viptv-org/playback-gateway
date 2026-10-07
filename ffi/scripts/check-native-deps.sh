#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
# Match both the default facade and the explicit Android native build graph.
for features in default torrent; do
  options=()
  if [[ "$features" == torrent ]]; then
    options=(--no-default-features --features torrent)
  fi
  graph="$(cargo tree --locked -p playback-gateway-ffi "${options[@]}" --edges normal --prefix none)"
  if grep -Eq '^(unrar-rs|axum) v' <<<"$graph"; then
    echo "Native $features dependency graph contains archive/control implementation." >&2
    exit 1
  fi
  feature_graph="$(cargo tree --locked -p playback-gateway-ffi "${options[@]}" --edges features --prefix none)"
  if grep -Fq 'feature "test-network-policy"' <<<"$feature_graph"; then
    echo "Native artifact graph enables the isolated fixture policy." >&2
    exit 1
  fi
  echo "Verified native $features dependency graph excludes archive/control crates."
done
