#!/usr/bin/env bash
# Build the helix-relay, gloas_inject and simulator-stub images from a Helix ref.
#
#   build-images.sh <helix-ref> [inject-ref]
#
# helix-relay is built from <helix-ref>. The injector is the crates/test-harness of
# [inject-ref] (default harness/gloas-inject) laid over the same tree, so it is
# compiled against the types of the relay under test. Both refs are resolved in
# HELIX_REPO, which is only read: the build runs in a shared clone under BUILD_DIR.
# The tags are written to $RUN_DIR/images.env, which the other scripts source.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

HELIX_REF="${1:?usage: build-images.sh <helix-ref> [inject-ref]}"
INJECT_REF="${2:-${INJECT_REF:-harness/gloas-inject}}"
HELIX_REPO="${HELIX_REPO:-$REPO_ROOT/../helix-epbs-stream}"
BUILD_DIR="${BUILD_DIR:-$RUN_DIR/build}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$BUILD_DIR/target}"

git -C "$HELIX_REPO" rev-parse --git-dir >/dev/null 2>&1 || die "HELIX_REPO=$HELIX_REPO is not a git checkout"
HELIX_SHA="$(git -C "$HELIX_REPO" rev-parse --verify "$HELIX_REF^{commit}")"
INJECT_SHA="$(git -C "$HELIX_REPO" rev-parse --verify "$INJECT_REF^{commit}")"
TAG="epbs-stream-${HELIX_SHA:0:8}"
SRC="$BUILD_DIR/helix-${HELIX_SHA:0:8}"
CTX="$BUILD_DIR/img-${HELIX_SHA:0:8}"
echo "helix  $HELIX_REF = $HELIX_SHA"
echo "inject $INJECT_REF = $INJECT_SHA"

log "source tree $SRC"
rm -rf "$SRC"
mkdir -p "$BUILD_DIR"
# A real checkout, not an archive: helix's build.rs stamps `git rev-parse HEAD` into the
# relay's info metric, so the image reports the commit it was built from.
git clone -q --shared --no-checkout "$HELIX_REPO" "$SRC"
git -C "$SRC" checkout -q --detach "$HELIX_SHA"
rm -rf "$SRC/crates/test-harness"
git -C "$SRC" checkout -q "$INJECT_SHA" -- crates/test-harness

log "cargo build --release (target $CARGO_TARGET_DIR)"
# cd so rustup picks up the tree's rust-toolchain.toml
(cd "$SRC" && cargo build --release -p helix-relay --bin helix-relay -p helix-test-harness --bin gloas_inject)

log "docker images"
rm -rf "$CTX" && mkdir -p "$CTX"
cp "$CARGO_TARGET_DIR/release/helix-relay" "$CARGO_TARGET_DIR/release/gloas_inject" "$HARNESS/stub/simstub.py" "$CTX/"
HELIX_IMAGE="local/helix-relay:$TAG"
INJECT_IMAGE="local/helix-inject:$TAG"
for target in relay inject stub; do
  case $target in
    relay) image=$HELIX_IMAGE ;;
    inject) image=$INJECT_IMAGE ;;
    stub) image=$STUB_IMAGE ;;
  esac
  docker build -q --target "$target" -t "$image" -f "$HARNESS/images/Dockerfile" \
    --label "epbs-stream.helix-commit=$HELIX_SHA" --label "epbs-stream.inject-commit=$INJECT_SHA" "$CTX" >/dev/null
  echo "built $image"
done

cat > "$RUN_DIR/images.env" <<EOF
# written by build-images.sh $HELIX_REF $INJECT_REF
: "\${HELIX_IMAGE:=$HELIX_IMAGE}"
: "\${INJECT_IMAGE:=$INJECT_IMAGE}"
: "\${STUB_IMAGE:=$STUB_IMAGE}"
EOF
echo "wrote $RUN_DIR/images.env"
