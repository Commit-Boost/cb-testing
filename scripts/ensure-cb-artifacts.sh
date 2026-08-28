#!/usr/bin/env bash
# ensure-cb-artifacts.sh - make the ePBS commit-boost artifacts reproducible.
#
# The ePBS sim needs two commit-boost artifacts that are NOT pullable: the CB
# sidecar image (with the ePBS bid pipe) and the `cb-km` binary (the mux ->
# keymanager projector). Both are built from the pinned `commit-boost-client`
# submodule and TAGGED BY ITS COMMIT SHA, so:
#   - a clone reproduces them from `git submodule update --init` + one build, and
#   - a stale artifact can never be silently used (the tag encodes the source
#     commit; a mismatched submodule bump forces a rebuild). This is the guard
#     for the exact trap that a stale cb-km binary caused: it kept projecting
#     builder_pubkeys from the relay URL long after the source stopped, and every
#     builder bid was rejected. Pinning to the submodule commit closes that hole.
#
# Prints two eval-able lines on stdout (all human output goes to stderr):
#   CB_IMAGE=commit-boost/commit-boost:km-e2e-<sha>
#   CB_KM_BIN=<abs path to the submodule's cb-km>
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sub="$root/commit-boost-client"

if [[ ! -f "$sub/Cargo.toml" ]]; then
  echo "commit-boost-client submodule is not initialized." >&2
  echo "  run: git submodule update --init --recursive" >&2
  exit 1
fi

# Init NESTED submodules too (commit-boost-client has crates/signer/proto). Without
# them the CB build fails on a missing protoc file - a "-" prefix in `git submodule
# status`. Idempotent; also re-syncs after a submodule bump.
git -C "$sub" submodule update --init --recursive 1>&2

sha="$(git -C "$sub" rev-parse --short HEAD)"
img="commit-boost/commit-boost:km-e2e-$sha"
kmbin="$sub/target/release/cb-km"

# 1. CB sidecar image, tagged by the submodule commit. Build from the submodule
#    if the sha-tagged image is absent (first run, or after a submodule bump).
if docker image inspect "$img" >/dev/null 2>&1; then
  echo "cb image ok: $img" >&2
else
  echo "building $img from commit-boost-client @ $sha (first run for this commit; ~a few min)..." >&2
  ( cd "$sub" && just build-all "km-e2e-$sha" ) 1>&2
fi

# 2. cb-km binary, from the same submodule. Rebuild if missing or if any km-tool
#    source is newer than the binary (the freshness guard).
stale_src=""
[[ -x "$kmbin" ]] && stale_src="$(find "$sub/crates/km-tool/src" -name '*.rs' -newer "$kmbin" 2>/dev/null | head -1 || true)"
if [[ ! -x "$kmbin" || -n "$stale_src" ]]; then
  echo "building cb-km from commit-boost-client @ $sha..." >&2
  ( cd "$sub" && cargo build -p cb-km-tool --release ) 1>&2
else
  echo "cb-km ok: $kmbin" >&2
fi

echo "CB_IMAGE=$img"
echo "CB_KM_BIN=$kmbin"
