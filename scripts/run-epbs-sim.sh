#!/usr/bin/env bash
# run-epbs-sim.sh - reproducible ePBS (gloas) + commit-boost + keymanager loop.
#
# Stands up a gloas devnet with buildoor (gloas_fork_epoch 0, minimal, 6s slots),
# lodestar CL+VC with keymanager enabled, adds a commit-boost PBS sidecar as a
# first-class kurtosis enclave service (CB_LAUNCH=service; docker = legacy path),
# runs `cb-km apply` to point every validator's builder_config at commit-boost
# (auth_data=buildoor), then observes the loop and asserts:
#   BN -> CB execution_payload_bid calls, buildoor bids via CB (auction winner),
#   and builder-built blocks on chain (signed_execution_payload_bid.value != 0).
#
# A real ethereum-package `epbs` mev_type that launches CB as the sidecar (no
# manual add + no `cb-km apply`) needs an upstream-package change - see the
# "Native ePBS mev_type: investigation & submodule-upgrade blockers" in docs/EPBS.md.
#
# Opt-in assertion modes turn the two merged keymanager features into live
# regression checks (both are cb-km-driven; CB just routes):
#   --assert p2p       also assert the min_bid p2p floor. buildoor's p2p-bidder
#                      publishes a 101000000 Gwei bid every slot, below the
#                      200000000 Gwei floor, so the BN floors it on every
#                      builder-built slot ("Ignoring p2p bid below min bid").
#                      HARD-fails unless the floor fires (>=1 rejection), CB is
#                      the selected bidSource (>=1), and no p2p bid ever wins (0).
#   --assert preserve  after `cb-km apply`, POST a third-party builder_config
#                      entry to a key, then `cb-km apply --preserve-entries` and
#                      assert BOTH our entry and the third-party entry survive
#                      (a plain apply drops it). Keymanager-only: skips the
#                      builder-activation wait + observe window.
# No flag = the default builder-built assertion (unchanged).
#
# One devnet at a time (~15G). Usage:
#   scripts/run-epbs-sim.sh [--assert p2p|preserve]
# (or `just epbs-sim` / `just epbs-sim-assert p2p`).
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
# How commit-boost joins the devnet:
#   service (default) = a first-class kurtosis enclave service (proper enclave DNS,
#                       one `kurtosis enclave rm` teardown, shows in `enclave inspect`).
#   docker            = legacy raw `docker run` on the enclave network (fallback).
CB_LAUNCH="${CB_LAUNCH:-service}"
CB_ARTIFACT="${CB_ARTIFACT:-cb-epbs-config}"  # kurtosis files-artifact name (service path)
CB_KM_BIN="${CB_KM_BIN:-}"
BUILDOOR_ACTIVATION_TIMEOUT="${BUILDOOR_ACTIVATION_TIMEOUT:-1200}"  # s to wait for the builder deposit to activate (queue delay varies run to run, seen out to ~slot 100)
OBSERVE_SLOTS="${OBSERVE_SLOTS:-16}"     # slots to watch once buildoor is active
MIN_BUILDER_SLOTS="${MIN_BUILDER_SLOTS:-8}"  # PASS threshold (builder-built via CB; allows some missed slots)
KEEP="${KEEP:-0}"                        # 1 = leave the enclave + CB running
RUN_DIR="configs/epbs/.run"              # rendered (gitignored) config lives here

# opt-in assertion mode (default = today's builder-built assertion)
ASSERT_MODE="${ASSERT_MODE:-}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --assert)   ASSERT_MODE="${2:-}"; shift 2 ;;
    --assert=*) ASSERT_MODE="${1#*=}"; shift ;;
    -h|--help)  sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done
case "$ASSERT_MODE" in
  ""|p2p|preserve) : ;;
  *) printf 'unknown --assert mode: %s (want p2p|preserve)\n' "$ASSERT_MODE" >&2; exit 2 ;;
esac

log()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
die()  { printf '\n\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

# ---- CB launch abstraction (service | docker) -------------------------------
# The commit-boost sidecar is read from a few places (boot check, buildoor
# activation poll, final assertions); route those through one shim so both the
# first-class enclave-service path and the legacy docker path share the logic.
# `-a` (all logs) is load-bearing: `kurtosis service logs` defaults to the last
# 200 lines, and the BN emits far more than that per slot at debug level, so a
# plain tail almost never contains the once-per-slot bid-selection lines the
# asserts grep for. Reading the full history makes the counts deterministic.
cb_logs() {
  if [[ "$CB_LAUNCH" == "service" ]]; then
    kurtosis service logs -a "$ENCLAVE" "$CB_NAME" 2>&1 || true
  else
    docker logs "$CB_NAME" 2>&1 || true
  fi
}

# Beacon node logs: bid selection (the min_bid p2p floor + the winning bidSource)
# is logged by the BN's produceBlockV4 path, not the VC. cl_log_level=debug in the
# args file surfaces the debug-level "Ignoring p2p bid below min bid" line. `-a`
# for the same reason as cb_logs: the rejection line lives in the full history,
# not the 200-line tail (the tail hiding it was the sole cause of the earlier
# "floor unexercised" flake).
bn_logs() { kurtosis service logs -a "$ENCLAVE" cl-1-lodestar-geth 2>&1 || true; }

# ---- keymanager helpers (used out-of-enclave, exactly like cb-km) -----------
# Authenticated GET/POST of a validator's builder_config, against the same
# exposed VC keymanager port + static token cb-km uses (see docs/EPBS.md,
# "How the keymanager calls happen"). KM_TOKEN/VC_KM are set before use.
km_get()  { curl -sf -H "Authorization: Bearer $KM_TOKEN" "$VC_KM/eth/v1/validator/$1/builder_config"; }
km_post() { # $1=pubkey $2=json-body -> prints the HTTP status code
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer $KM_TOKEN" -H 'Content-Type: application/json' \
    -d "$2" "$VC_KM/eth/v1/validator/$1/builder_config"
}
# the entry URLs stored on a key (one per line)
km_entry_urls() {
  km_get "$1" | python3 -c \
    "import sys,json;d=json.load(sys.stdin).get('data',{});[print(e.get('url','')) for e in (d.get('builders') or [])]" \
    2>/dev/null || true
}

# --- assert preserve: --preserve-entries keeps a third-party builder entry ----
# A plain `cb-km apply` full-replaces each key's builder_config, so an entry a
# THIRD-PARTY writer pinned is erased. `--preserve-entries` GETs first and folds
# such entries back in. This proves both halves live: a control key shows plain
# apply dropping the entry; the preserve key shows both entries surviving.
assert_preserve() {
  log "assert preserve: third-party builder_config entry survives --preserve-entries"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  local CB_URL="http://cb-epbs:18550"
  # two deterministic mux keys from configs/epbs/cb-config.toml.tmpl
  local K1="0x81b676591b823270a3284ace7d81cbce2d6cdce55bb0e053874d7e3a08f729453009d3e662ec3130379f43c0f3210b6d"
  local K2="0x81ea9f74ef7d935b807474e38954ae3934856219a23e074954b2e860c5a3c400f9aedb42cd27cb4ceb697ca36d1e58cb"
  # a distinct third-party writer's entry (distinct url + auth_data); buildoor's
  # pubkey is reused only as a valid BLS point for builder_pubkeys.
  local TP_URL="http://third-party-relay:19999"
  local TP_AUTH="0x74686972647061727479"   # "thirdparty"
  local TP_BP="0x8de7ec501d574152f52a962bf588573df2fc3563fd0c6077651208ed20f24f3d8572425706b343117b48bdca56808416"
  local TP_DOC
  TP_DOC="{\"builders\":[{\"url\":\"$TP_URL\",\"auth_data\":\"$TP_AUTH\",\"builder_pubkeys\":[\"$TP_BP\"]}]}"

  # sanity: our apply put the CB entry on both keys
  km_entry_urls "$K1" | grep -qF "$CB_URL" || die "preserve: CB entry not on $K1 after apply"
  km_entry_urls "$K2" | grep -qF "$CB_URL" || die "preserve: CB entry not on $K2 after apply"

  # --- CONTROL: a plain apply DROPS a third-party entry (K2) ---
  log "control: plain apply drops a third-party entry"
  local code
  code="$(km_post "$K2" "$TP_DOC")"; [[ "$code" == 2* ]] || die "control: POST third-party to K2 not accepted (HTTP $code)"
  km_entry_urls "$K2" | grep -qF "$TP_URL" || die "control: third-party entry not stored on K2"
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml"
  km_entry_urls "$K2" | grep -qF "$CB_URL" || die "control: CB entry missing on K2 after plain apply"
  ! km_entry_urls "$K2" | grep -qF "$TP_URL" || die "control: plain apply did NOT drop the third-party entry"
  echo "  control OK: plain apply replaced K2 with only the CB entry (third-party dropped)"

  # --- PRESERVE: apply --preserve-entries KEEPS a third-party entry (K1) ---
  log "preserve: apply --preserve-entries keeps a third-party entry"
  code="$(km_post "$K1" "$TP_DOC")"; [[ "$code" == 2* ]] || die "preserve: POST third-party to K1 not accepted (HTTP $code)"
  km_entry_urls "$K1" | grep -qF "$TP_URL" || die "preserve: third-party entry not stored on K1"
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --preserve-entries
  km_entry_urls "$K1" | grep -qF "$CB_URL" || die "preserve: CB entry missing on K1 after --preserve-entries"
  km_entry_urls "$K1" | grep -qF "$TP_URL" || die "preserve: third-party entry DROPPED by --preserve-entries (regression)"
  echo "  preserve OK: K1 carries BOTH the CB entry and the third-party entry"

  log "RESULT"
  printf '\033[1;32mPASS: --preserve-entries kept the third-party builder_config entry; plain apply dropped it\033[0m\n'
}

# ---- cleanup ----------------------------------------------------------------
cleanup() {
  local rc=$?
  if [[ "$KEEP" == "1" ]]; then
    log "KEEP=1 - leaving enclave '$ENCLAVE' and CB sidecar '$CB_NAME' running"
  else
    log "cleanup: removing enclave (+ CB sidecar)"
    # service path: the CB service is inside the enclave, torn down with it.
    # docker path: the CB container is a separate object, remove it explicitly.
    [[ "$CB_LAUNCH" == "docker" ]] && docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
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
  if [[ -x /home/j/code/commit-boost-client/target/release/cb-km ]]; then CB_KM_BIN=/home/j/code/commit-boost-client/target/release/cb-km
  elif command -v cb-km >/dev/null 2>&1; then CB_KM_BIN="$(command -v cb-km)"
  elif [[ -x /home/j/code/cb-km-wt1/target/release/cb-km ]]; then CB_KM_BIN=/home/j/code/cb-km-wt1/target/release/cb-km
  else die "cb-km binary not found - set CB_KM_BIN (build: cargo build -p cb-km-tool --release)"; fi
fi
"$CB_KM_BIN" --help >/dev/null 2>&1 || die "cb-km at $CB_KM_BIN not runnable"
# preserve mode needs the merged --preserve-entries flag (epbs branch)
if [[ "$ASSERT_MODE" == "preserve" ]]; then
  "$CB_KM_BIN" apply --help 2>&1 | grep -q -- '--preserve-entries' \
    || die "cb-km at $CB_KM_BIN lacks --preserve-entries; build the epbs branch: \
(cd /home/j/code/commit-boost-client && cargo build -p cb-km-tool --release) and set CB_KM_BIN"
fi
avail_g=$(free -g | awk '/^Mem:/{print $7}')
(( avail_g >= 10 )) || echo "WARN: only ${avail_g}G RAM available; a devnet wants ~15G"
echo "cb-km:   $CB_KM_BIN"
echo "CB img:  $CB_IMAGE"
echo "CB join: $CB_LAUNCH"
echo "assert:  ${ASSERT_MODE:-default (builder-built)}"
echo "enclave: $ENCLAVE   observe: ${OBSERVE_SLOTS} slots   pass>=${MIN_BUILDER_SLOTS}"

# clean any stale run (docker-path container is a separate object; the enclave rm
# clears a service-path CB with the rest of the enclave)
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
# Strict-shape both before they reach sed: they interpolate into a config the CB
# process trusts, so a malformed BN response must fail loud, not inject.
[[ "$GEN_TIME" =~ ^[0-9]+$ ]] || die "genesis_time is not a plain integer: '$GEN_TIME'"
[[ "$GVR" =~ ^0x[0-9a-fA-F]{64}$ ]] || die "genesis_validators_root is not a 32-byte hex root: '$GVR'"
echo "genesis_time=$GEN_TIME  genesis_validators_root=$GVR"

rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
# km-token.txt is the well-known static ethereum-package keymanager token (public,
# sim-only). It is NOT a secret and must never be reused against a real VC. See docs/EPBS.md.
cp configs/epbs/km-token.txt "$RUN_DIR/km-token.txt"
sed -e "s|__GENESIS_TIME__|$GEN_TIME|" \
    -e "s|__GENESIS_VALIDATORS_ROOT__|$GVR|" \
    configs/epbs/cb-config.toml.tmpl > "$RUN_DIR/cb-config.toml"
sed -e "s|__VC_KM_URL__|$VC_KM|" \
    -e "s|__TOKEN_PATH__|$(pwd)/$RUN_DIR/km-token.txt|" \
    configs/epbs/km-overlay.toml.tmpl > "$RUN_DIR/km-overlay.toml"
grep -q '__' "$RUN_DIR/cb-config.toml" && die "unrendered placeholder in cb-config.toml"

# ---- 4. place commit-boost PBS in the loop (VC -> CB -> buildoor) ------------
if [[ "$CB_LAUNCH" == "service" ]]; then
  log "adding commit-boost PBS as a first-class enclave service ($CB_NAME)"
  # Upload the rendered CB config as a kurtosis files-artifact and add CB as a
  # real enclave service: it gets enclave DNS ($CB_NAME resolvable by the VC and
  # buildoor by name), is torn down by `kurtosis enclave rm`, and shows up in
  # `kurtosis enclave inspect` - no manual container to track.
  kurtosis files upload "$ENCLAVE" "$RUN_DIR" --name "$CB_ARTIFACT" >/dev/null \
    || die "kurtosis files upload of $RUN_DIR failed"
  # NOTE: --env is ONE comma-separated string, so RUST_LOG must not contain a
  # comma; RUST_LOG=debug gives the cb_pbs bid/auction lines the asserts grep for.
  kurtosis service add "$ENCLAVE" "$CB_NAME" "$CB_IMAGE" \
    --files "/cb:$CB_ARTIFACT" \
    --env "CB_CONFIG=/cb/cb-config.toml,RUST_LOG=debug" \
    --ports "pbs=http:18550" \
    --cmd pbs >/dev/null \
    || { cb_logs | tail -20; die "kurtosis service add for CB failed"; }
  sleep 4
  kurtosis service inspect "$ENCLAVE" "$CB_NAME" >/dev/null 2>&1 \
    || { cb_logs | tail -20; die "CB service not present after add"; }
  echo "CB service added: $CB_NAME (pbs:18550) in enclave $ENCLAVE"
else
  log "starting commit-boost PBS via docker ($CB_NAME on $NET)"
  docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
  docker run -d --name "$CB_NAME" --network "$NET" \
    -v "$(pwd)/$RUN_DIR:/cb:ro" \
    -e CB_CONFIG=/cb/cb-config.toml \
    -e RUST_LOG=info,cb_pbs=debug \
    "$CB_IMAGE" pbs >/dev/null
  sleep 4
  docker ps --format '{{.Names}}' | grep -qx "$CB_NAME" || { docker logs "$CB_NAME" 2>&1 | tail -20; die "CB container exited on boot"; }
  echo "CB up: $(docker ps --filter name=^${CB_NAME}$ --format '{{.Status}}')"
fi

# ---- 5. apply the keymanager builder_config (route VC -> CB) -----------------
log "cb-km apply (point 64 validators' builder_config at CB)"
"$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml"
echo "apply OK"

# preserve mode is a pure keymanager-API check: it needs the applied docs but not
# the builder loop, so run it now and finish (skip activation wait + observe).
if [[ "$ASSERT_MODE" == "preserve" ]]; then
  assert_preserve
  exit 0
fi

# ---- 6a. wait for buildoor to become active on chain -------------------------
# buildoor submits a builder DEPOSIT to the EIP-8282 registry on boot and only
# bids once that deposit is included AND activated (an activation-queue delay,
# empirically ~epoch 4 / slot ~33 on minimal). Until then CB gets 204 "no header
# available". Poll CB's log for buildoor's first bid before observing.
log "waiting for buildoor activation (builder deposit -> registry -> active; ~epoch 4)"
buildoor_live=0
for i in $(seq 1 $(( BUILDOOR_ACTIVATION_TIMEOUT / 6 )) ); do
  # key on info-level signals (per-header receipt is debug and may be filtered):
  # either buildoor's header reached CB or it won CB's auction.
  if cb_logs | grep -qE 'auction winner.*buildoor-mux|received (new )?header.*buildoor-mux'; then buildoor_live=1; break; fi
  cur=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
  printf '  head=%s waiting for first buildoor bid via CB...\r' "$cur"
  sleep 6
done
echo
(( buildoor_live == 1 )) || { cb_logs | tail -15; die "buildoor never bid through CB within ${BUILDOOR_ACTIVATION_TIMEOUT}s"; }
echo "buildoor is active - first bid seen"

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
CB_LOG=$(cb_logs)
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

# ---- 7b. p2p-floor assertion (opt-in) ---------------------------------------
# The rendered CB config sets min_bid_p2p_eth = "0.2" (top-level [pbs]), which
# cb-km projects as the key-level min_bid (200000000 Gwei), ABOVE buildoor's
# 101000000 Gwei p2p bid, while the CB (builder-API) entry keeps min_bid = 0 so
# CB bids survive. buildoor's p2p-bidder publishes a competing bid every slot
# (POSTed to the BN's publishExecutionPayloadBid endpoint, which pools it AND
# gossips it: "Published execution payload bid ... value=101000000"), so the BN's
# produceBlockV4 floors it on every builder-built slot:
#   "Ignoring p2p bid below min bid slot=.. bidValue=101000000 minBid=200000000".
# A floored p2p bid is nulled BEFORE the candidate ranking, so it never shows up
# in "Ranked builder bid candidates"; the rejection line is the floor's only
# signal (do NOT gate on a p2p ranked-candidate; there is none when the floor
# works). The guarantee, asserted HARD from the BN's full bid-selection log:
# the floor FIRES (>=1 rejection), CB is the selected source (>=1), and no p2p
# bid ever wins (0).
p2p_ok=1
if [[ "$ASSERT_MODE" == "p2p" ]]; then
  log "assert p2p: min_bid p2p floor rejects buildoor's competing p2p bid; CB is selected"
  BN_LOG="$(bn_logs)"
  p2p_rejected=$(grep -c 'Ignoring p2p bid below min bid' <<<"$BN_LOG" || true)
  cb_selected=$(grep 'Selected builder block' <<<"$BN_LOG" | grep -c 'bidSource=[^, ]*cb-epbs' || true)
  p2p_selected=$(grep 'Selected builder block' <<<"$BN_LOG" | grep -c 'bidSource=[^, ]*p2p' || true)
  sample=$(grep -m1 'Ignoring p2p bid below min bid' <<<"$BN_LOG" || true)
  echo "  p2p bids rejected below floor:    $p2p_rejected"
  [[ -n "$sample" ]] && echo "    e.g. ${sample#*: }"
  echo "  blocks selected via CB bidSource: $cb_selected"
  echo "  blocks selected via p2p bidSource:$p2p_selected"
  if (( p2p_rejected < 1 )); then
    p2p_ok=0   # the floor never fired: buildoor's p2p bid was not rejected this run
    echo "  the p2p floor never fired (no 'Ignoring p2p bid below min bid'); check buildoor's p2p-bidder"
    bn_logs | grep -E 'bid below min bid|Selected (builder|local) block|Ranked builder bid' | tail -30
  elif (( cb_selected < 1 || p2p_selected > 0 )); then
    p2p_ok=0   # real breach: CB never won, or a p2p bid WON despite the floor
    bn_logs | grep -E 'bid below min bid|Selected (builder|local) block|Ranked builder bid' | tail -30
  else
    echo "  p2p floor PROVEN: $p2p_rejected competing p2p bid(s) rejected below the floor; CB won every selection"
  fi
fi

log "RESULT"
if (( n_built >= MIN_BUILDER_SLOTS && auction_wins >= 1 && bid_calls >= 1 && p2p_ok == 1 )); then
  printf '\033[1;32mPASS: %s/%s observed slots builder-built via commit-boost (buildoor)\033[0m\n' \
    "$n_built" "$OBSERVE_SLOTS"
  if [[ "$ASSERT_MODE" == "p2p" ]]; then
    printf '\033[1;32mPASS: p2p floor PROVEN (%s rejected); %s blocks selected CB, 0 selected p2p\033[0m\n' \
      "$p2p_rejected" "$cb_selected"
  fi
  exit 0
else
  cb_logs | tail -30
  (( p2p_ok == 1 )) || die "p2p assertion failed (rejected=$p2p_rejected need>=1, cb_selected=$cb_selected need>=1, p2p_selected=$p2p_selected need=0)"
  die "loop not satisfied (built=$n_built need>=$MIN_BUILDER_SLOTS, wins=$auction_wins, bids=$bid_calls)"
fi
