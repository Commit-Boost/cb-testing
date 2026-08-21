#!/usr/bin/env bash
# run-epbs-sim.sh — reproducible ePBS (gloas) + commit-boost + keymanager loop.
#
# Stands up a gloas devnet with buildoor (gloas_fork_epoch 0, minimal, 6s slots),
# lodestar CL+VC with keymanager enabled, inserts a commit-boost PBS sidecar into
# the enclave network, runs `cb-km apply` to point every validator's builder_config
# at commit-boost (auth_data=buildoor), then observes the loop and asserts:
#   BN -> CB execution_payload_bid calls, buildoor bids via CB (auction winner),
#   and builder-built blocks on chain (signed_execution_payload_bid.value != 0).
#
# This is the epbs-branch SCRATCH harness (manual CB-insert) pending a real
# ethereum-package `epbs` mev_type that launches CB as the sidecar. See docs/EPBS.md.
#
# One devnet at a time (~15G). Usage: scripts/run-epbs-sim.sh  (or `just epbs-sim`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ---- knobs (env-overridable) ------------------------------------------------
ENCLAVE="${ENCLAVE:-epbs-sim}"
# Upstream ethereum-package (kurtosis fetches + caches it). The repo's pinned
# submodule predates gloas-genesis compatibility with the local/lodestar:km image
# (see docs/EPBS.md), so this harness uses upstream. Pin with EP_PACKAGE=...@<ref>
# or point at a local checkout ($REPO_ROOT/ethereum-package) once it is upgraded.
EP_PACKAGE="${EP_PACKAGE:-github.com/ethpandaops/ethereum-package}"
ARGS_FILE="${ARGS_FILE:-configs/epbs/gloas-epbs.yaml}"
CB_IMAGE="${CB_IMAGE:-commit-boost/commit-boost:km-e2e}"
CB_NAME="${CB_NAME:-cb-epbs}"            # must match advertised host in the templates
CB_KM_BIN="${CB_KM_BIN:-}"
BUILDOOR_ACTIVATION_TIMEOUT="${BUILDOOR_ACTIVATION_TIMEOUT:-600}"  # s to wait for the builder deposit to activate
OBSERVE_SLOTS="${OBSERVE_SLOTS:-16}"     # slots to watch once buildoor is active
MIN_BUILDER_SLOTS="${MIN_BUILDER_SLOTS:-8}"  # PASS threshold (builder-built via CB; allows some missed slots)
KEEP="${KEEP:-0}"                        # 1 = leave the enclave + CB running
RUN_DIR="configs/epbs/.run"              # rendered (gitignored) config lives here

log()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
die()  { printf '\n\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

# ---- cleanup ----------------------------------------------------------------
cleanup() {
  local rc=$?
  if [[ "$KEEP" == "1" ]]; then
    log "KEEP=1 — leaving enclave '$ENCLAVE' and container '$CB_NAME' running"
  else
    log "cleanup: removing CB container + enclave"
    docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
    kurtosis enclave rm -f "$ENCLAVE" >/dev/null 2>&1 || true
  fi
  exit $rc
}
trap cleanup EXIT

# ---- preflight --------------------------------------------------------------
log "preflight"
command -v kurtosis >/dev/null || die "kurtosis not on PATH"
command -v docker   >/dev/null || die "docker not on PATH"
[[ -f "$ARGS_FILE" ]] || die "args file $ARGS_FILE missing"
case "$EP_PACKAGE" in
  github.com/*|http*) : ;;                                  # remote ref: kurtosis fetches it
  *) [[ -d "$EP_PACKAGE" ]] || die "ethereum-package path $EP_PACKAGE missing" ;;
esac
docker image inspect "$CB_IMAGE" >/dev/null 2>&1 || die "CB image $CB_IMAGE not present locally"

if [[ -z "$CB_KM_BIN" ]]; then
  if command -v cb-km >/dev/null 2>&1; then CB_KM_BIN="$(command -v cb-km)"
  elif [[ -x /home/j/code/cb-km-wt1/target/release/cb-km ]]; then CB_KM_BIN=/home/j/code/cb-km-wt1/target/release/cb-km
  else die "cb-km binary not found — set CB_KM_BIN (build: cargo build -p cb-km-tool --release)"; fi
fi
"$CB_KM_BIN" --help >/dev/null 2>&1 || die "cb-km at $CB_KM_BIN not runnable"
avail_g=$(free -g | awk '/^Mem:/{print $7}')
(( avail_g >= 10 )) || echo "WARN: only ${avail_g}G RAM available; a devnet wants ~15G"
echo "cb-km:   $CB_KM_BIN"
echo "CB img:  $CB_IMAGE"
echo "enclave: $ENCLAVE   observe: ${OBSERVE_SLOTS} slots   pass>=${MIN_BUILDER_SLOTS}"

# clean any stale run
docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
kurtosis enclave rm -f "$ENCLAVE" >/dev/null 2>&1 || true

# ---- 1. launch the gloas devnet ---------------------------------------------
log "launching gloas devnet ($ARGS_FILE)"
kurtosis run "$EP_PACKAGE" --enclave "$ENCLAVE" --args-file "$ARGS_FILE" --image-download always

NET="kt-${ENCLAVE}"
BN="$(kurtosis port print "$ENCLAVE" cl-1-lodestar-geth http)"
VC_KM="$(kurtosis port print "$ENCLAVE" vc-1-geth-lodestar http-validator)"
[[ -n "$BN" && -n "$VC_KM" ]] || die "could not discover BN / VC keymanager ports"
echo "BN=$BN  VC-km=$VC_KM  net=$NET"

# ---- 2. wait for the beacon node + gloas activation -------------------------
log "waiting for beacon node + gloas head"
head_slot=0
for i in $(seq 1 60); do
  gen=$(curl -sf "$BN/eth/v1/beacon/genesis" 2>/dev/null || true)
  [[ -n "$gen" ]] || { sleep 3; continue; }
  head_slot=$(curl -sf "$BN/eth/v1/beacon/headers/head" 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
  (( head_slot >= 1 )) && break
  sleep 3
done
(( head_slot >= 1 )) || die "beacon node did not advance past genesis"
echo "head slot = $head_slot"

# ---- 3. render CB config + overlay from the live chain -----------------------
log "rendering CB config from live BN"
GEN_TIME=$(curl -sf "$BN/eth/v1/beacon/genesis" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['genesis_time'])")
GVR=$(curl -sf "$BN/eth/v1/beacon/genesis" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['genesis_validators_root'])")
[[ -n "$GEN_TIME" && "$GVR" == 0x* ]] || die "could not read genesis_time / genesis_validators_root"
echo "genesis_time=$GEN_TIME  genesis_validators_root=$GVR"

rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
cp configs/epbs/km-token.txt "$RUN_DIR/km-token.txt"
sed -e "s|__GENESIS_TIME__|$GEN_TIME|" \
    -e "s|__GENESIS_VALIDATORS_ROOT__|$GVR|" \
    configs/epbs/cb-config.toml.tmpl > "$RUN_DIR/cb-config.toml"
sed -e "s|__VC_KM_URL__|$VC_KM|" \
    -e "s|__TOKEN_PATH__|$(pwd)/$RUN_DIR/km-token.txt|" \
    configs/epbs/km-overlay.toml.tmpl > "$RUN_DIR/km-overlay.toml"
grep -q '__' "$RUN_DIR/cb-config.toml" && die "unrendered placeholder in cb-config.toml"

# ---- 4. insert commit-boost PBS into the enclave network --------------------
log "starting commit-boost PBS ($CB_NAME on $NET)"
docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
docker run -d --name "$CB_NAME" --network "$NET" \
  -v "$(pwd)/$RUN_DIR:/cb:ro" \
  -e CB_CONFIG=/cb/cb-config.toml \
  -e RUST_LOG=info,cb_pbs=debug \
  "$CB_IMAGE" pbs >/dev/null
sleep 4
docker ps --format '{{.Names}}' | grep -qx "$CB_NAME" || { docker logs "$CB_NAME" 2>&1 | tail -20; die "CB container exited on boot"; }
echo "CB up: $(docker ps --filter name=^${CB_NAME}$ --format '{{.Status}}')"

# ---- 5. apply the keymanager builder_config (route VC -> CB) -----------------
log "cb-km apply (point 64 validators' builder_config at CB)"
"$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml"
echo "apply OK"

# ---- 6a. wait for buildoor to become active on chain -------------------------
# buildoor submits a builder DEPOSIT to the EIP-8282 registry on boot and only
# bids once that deposit is included AND activated (an activation-queue delay,
# empirically ~epoch 4 / slot ~33 on minimal). Until then CB gets 204 "no header
# available". Poll CB's log for buildoor's first bid before observing.
log "waiting for buildoor activation (builder deposit -> registry -> active; ~epoch 4)"
buildoor_live=0
for i in $(seq 1 $(( BUILDOOR_ACTIVATION_TIMEOUT / 6 )) ); do
  if docker logs "$CB_NAME" 2>&1 | grep -q 'received new header.*buildoor-mux'; then buildoor_live=1; break; fi
  cur=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
  printf '  head=%s waiting for first buildoor bid via CB...\r' "$cur"
  sleep 6
done
echo
(( buildoor_live == 1 )) || { docker logs "$CB_NAME" 2>&1 | tail -15; die "buildoor never bid through CB within ${BUILDOOR_ACTIVATION_TIMEOUT}s"; }
echo "buildoor is active — first bid seen"

# ---- 6b. observe the loop ----------------------------------------------------
start_slot=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])")
end_slot=$(( start_slot + OBSERVE_SLOTS ))
log "observing slots ${start_slot}..${end_slot} ($(( OBSERVE_SLOTS * 6 ))s)"
for i in $(seq 1 $(( OBSERVE_SLOTS + 3 ))); do
  cur=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
  printf '  head=%s / target=%s\r' "$cur" "$end_slot"
  (( cur >= end_slot )) && break
  sleep 6
done
echo

# ---- 7. assert --------------------------------------------------------------
log "verifying the loop"
# CB-side: bid requests + buildoor auction wins in the observed window
CB_LOG=$(docker logs "$CB_NAME" 2>&1 || true)
bid_calls=$(grep -c 'execution_payload_bid' <<<"$CB_LOG" || true)
auction_wins=$(grep -c 'auction winner.*buildoor-mux' <<<"$CB_LOG" || true)

# chain-side: builder-built blocks (signed_execution_payload_bid.value != 0)
builder_built=$(python3 - "$BN" "$start_slot" "$end_slot" <<'PY'
import sys,json,urllib.request
bn,s0,s1=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
built=[]
for slot in range(s0,s1+1):
    try:
        with urllib.request.urlopen(f"{bn}/eth/v2/beacon/blocks/{slot}",timeout=5) as r:
            body=json.load(r)['data']['message']['body']
        bid=body.get('signed_execution_payload_bid',{}).get('message',{})
        if bid.get('value','0') not in ('0','',None):
            built.append(slot)
    except Exception:
        pass  # missed slot / 404
print(len(built))
print(",".join(map(str,built)))
PY
)
n_built=$(sed -n 1p <<<"$builder_built")
built_slots=$(sed -n 2p <<<"$builder_built")

echo "  BN -> CB bid calls (window+):     $bid_calls"
echo "  buildoor auction wins via CB:     $auction_wins"
echo "  builder-built blocks on chain:    $n_built  [slots: $built_slots]"

log "RESULT"
if (( n_built >= MIN_BUILDER_SLOTS && auction_wins >= 1 && bid_calls >= 1 )); then
  printf '\033[1;32mPASS: %s/%s observed slots builder-built via commit-boost (buildoor)\033[0m\n' \
    "$n_built" "$OBSERVE_SLOTS"
  exit 0
else
  docker logs "$CB_NAME" 2>&1 | tail -30
  die "loop not satisfied (built=$n_built need>=$MIN_BUILDER_SLOTS, wins=$auction_wins, bids=$bid_calls)"
fi
