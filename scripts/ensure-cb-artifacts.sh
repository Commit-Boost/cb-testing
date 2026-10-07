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
# --km-only provisions and prints only CB_KM_BIN (no docker), for
# `run-epbs-sim.sh --render-only`. --no-km provisions and prints only CB_IMAGE,
# for a CB commit without cb-km (`--assert manual-config`).
#
# CB_COMMIT=<commit-ish> builds both from that commit instead of the submodule's
# checkout, in a detached worktree of the submodule's repo at
# .cb-worktrees/<sha>, so the checkout other runs build from is left as it is.
# The commit must already be in the submodule's repo:
#   git -C commit-boost-client fetch <repo> <branch>
set -euo pipefail
km_only=0
no_km=0
case "${1:-}" in
  --km-only) km_only=1 ;;
  --no-km)   no_km=1 ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sub="$root/commit-boost-client"

if [[ ! -f "$sub/Cargo.toml" ]]; then
  echo "commit-boost-client submodule is not initialized." >&2
  echo "  run: git submodule update --init --recursive" >&2
  exit 1
fi

# The tree the artifacts are built from: the submodule's checkout, or CB_COMMIT's
# worktree when that is another commit.
src="$sub"
if [[ -n "${CB_COMMIT:-}" ]]; then
  want="$(git -C "$sub" rev-parse --verify --quiet "$CB_COMMIT^{commit}")" || {
    echo "CB_COMMIT=$CB_COMMIT is not a commit in $sub" >&2
    echo "  fetch it first: git -C $sub fetch <repo> <branch>" >&2
    exit 1
  }
  if [[ "$want" != "$(git -C "$sub" rev-parse HEAD)" ]]; then
    src="$root/.cb-worktrees/$want"
    [[ -e "$src/.git" ]] || git -C "$sub" worktree add --detach "$src" "$want" 1>&2
  fi
fi

# Init NESTED submodules too (commit-boost-client has crates/signer/proto). Without
# them the CB build fails on a missing protoc file - a "-" prefix in `git submodule
# status`. Idempotent; also re-syncs after a submodule bump.
git -C "$src" submodule update --init --recursive 1>&2

sha="$(git -C "$src" rev-parse --short HEAD)"
img="commit-boost/commit-boost:km-e2e-$sha"
kmbin="$src/target/release/cb-km"

# 1. CB sidecar image, tagged by the submodule commit. Build from the submodule
#    if the sha-tagged image is absent (first run, or after a submodule bump).
if (( km_only )); then
  :
elif docker image inspect "$img" >/dev/null 2>&1; then
  echo "cb image ok: $img" >&2
else
  echo "building $img from commit-boost-client @ $sha (first run for this commit; ~a few min)..." >&2
  ( cd "$src" && just build-all "km-e2e-$sha" ) 1>&2
fi
if (( no_km )); then
  echo "CB_IMAGE=$img"
  exit 0
fi

# 2. cb-km binary, from the same submodule. A stamp beside it records the commit
#    it was built from, so a submodule bump rebuilds it even when no cb-km file
#    changed (cb-km also parses the config through cb-common). A dirty tree always
#    goes through cargo, which rebuilds only what changed.
stamp="$kmbin.commit"
full_sha="$(git -C "$src" rev-parse HEAD)"
dirty="$(git -C "$src" status --porcelain --untracked-files=no)"
if [[ ! -x "$kmbin" || "$(cat "$stamp" 2>/dev/null)" != "$full_sha" || -n "$dirty" ]]; then
  echo "building cb-km from commit-boost-client @ $sha..." >&2
  # the package was cb-km-tool before it was renamed cb-km
  pkg=cb-km; [[ -d "$src/crates/km-tool" ]] && pkg=cb-km-tool
  ( cd "$src" && cargo build -p "$pkg" --release ) 1>&2
  # a dirty build is no commit's, so it leaves no stamp to match later
  if [[ -z "$dirty" ]]; then echo "$full_sha" > "$stamp"; else rm -f "$stamp"; fi
else
  echo "cb-km ok: $kmbin" >&2
fi

(( km_only )) || echo "CB_IMAGE=$img"
echo "CB_KM_BIN=$kmbin"
