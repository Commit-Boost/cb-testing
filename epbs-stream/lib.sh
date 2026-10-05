# Shared settings for the epbs-stream harness scripts. Source it, don't run it.
# Every path is relative to this directory unless overridden by an env var.

HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HARNESS/.." && pwd)"
RUN_DIR="${RUN_DIR:-$HARNESS/run}"
ENCLAVE="${ENCLAVE:-epbs-stream}"
mkdir -p "$RUN_DIR"

# build-images.sh records the tags it built here; explicit env vars still win.
# shellcheck disable=SC1091
[[ -f "$RUN_DIR/images.env" ]] && source "$RUN_DIR/images.env"
HELIX_IMAGE="${HELIX_IMAGE:-}"
INJECT_IMAGE="${INJECT_IMAGE:-}"
STUB_IMAGE="${STUB_IMAGE:-local/helix-simstub:epbs-stream}"
CB_IMAGE="${CB_IMAGE:-commit-boost/commit-boost:optiond-21fcef9}"
NETSHOOT_IMAGE="${NETSHOOT_IMAGE:-nicolaka/netshoot:latest}"
CAPTURE_NAME="${CAPTURE_NAME:-$ENCLAVE-pcap}"

log() { printf '\n==> [%s] %s\n' "$(date +%T)" "$*"; }
die() { echo "ERROR: $*" >&2; exit 1; }
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }

# Host URL of a service port, e.g. `port_url helix-relay api`.
port_url() { kurtosis port print "$ENCLAVE" "$1" "$2"; }

bn_url() { cat "$RUN_DIR/bn.url"; }

# Current wall-clock slot, from the BN's genesis time and the slot length in spec.json.
current_slot() {
  python3 - "$(bn_url)" "$RUN_DIR/spec.json" <<'EOF'
import json, sys, time, urllib.request
g = int(json.load(urllib.request.urlopen(sys.argv[1] + "/eth/v1/beacon/genesis"))["data"]["genesis_time"])
sps = int(json.load(open(sys.argv[2]))["data"]["SECONDS_PER_SLOT"])
print(int((time.time() - g) // sps))
EOF
}

# Sleep until `ms` milliseconds into `slot`.
wait_slot_ms() {
  python3 - "$(bn_url)" "$RUN_DIR/spec.json" "$1" "$2" <<'EOF'
import json, sys, time, urllib.request
g = int(json.load(urllib.request.urlopen(sys.argv[1] + "/eth/v1/beacon/genesis"))["data"]["genesis_time"])
sps = int(json.load(open(sys.argv[2]))["data"]["SECONDS_PER_SLOT"])
target = g + int(sys.argv[3]) * sps + int(sys.argv[4]) / 1000
time.sleep(max(0.0, target - time.time()))
EOF
}

# Docker container backing a Kurtosis service of this enclave.
service_container() {
  local uuid
  uuid="$(kurtosis service inspect "$ENCLAVE" "$1" --full-uuid 2>/dev/null | awk '/^UUID:/{print $2}')"
  [[ -n "$uuid" ]] || die "service $1 not found in enclave $ENCLAVE"
  echo "$1--$uuid"
}
