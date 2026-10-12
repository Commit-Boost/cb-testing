#!/usr/bin/env bash
# run-epbs-sim.sh - reproducible ePBS (gloas) + commit-boost loop.
#
# Stands up a gloas devnet with buildoor (gloas_fork_epoch 0, minimal, 6s slots)
# and one consensus client, adds a commit-boost PBS sidecar as a first-class
# kurtosis enclave service (CB_LAUNCH=service; docker = legacy path), and gives
# the validator client a proposer settings file that points every key at
# commit-boost (auth_data = buildoor's hostname), as the operator guide does:
# Lodestar --proposerSettingsFile, Prysm --proposer-settings-file, Lighthouse
# builder_definitions.yml in its validators dir. Then it observes the loop and
# asserts:
#   BN -> CB execution_payload_bid calls, buildoor bids via CB, and builder-built
#   blocks on chain whose bid is one CB returned (scripts/epbs-blocks.py and
#   scripts/epbs-sim-helpers.py via-cb).
# The file sets min_bid 0.15 ETH: under buildoor's builder-API bid, raised to
# 0.2 ETH, and over its 0.101 ETH p2p bid, so a block built from buildoor's bid
# came through CB.
#
# Opt-in assertion modes turn a merged feature into a live regression check:
#   --assert p2p       also assert the min_bid p2p floor: the BN floors
#                      buildoor's 0.101 ETH p2p bid on every builder-built slot
#                      ("Ignoring p2p bid below min bid"). HARD-fails unless the
#                      floor fires (>=1 rejection), CB is the selected bidSource
#                      (>=1), and no p2p bid ever wins (0).
#   --assert block-submission  also assert the blind-pipe reveal at POST
#                      /eth/v1/builder/beacon_blocks: the proposer reveals each
#                      builder-built block through CB, which forwards the body to
#                      buildoor undecoded. HARD-fails unless >=1 reveal reached
#                      the forward with zero 4xx and zero forwards buildoor never
#                      answered.
#   --assert builder-down  after buildoor activates and >=1 builder-built slot is
#                      seen, STOP buildoor and assert the proposer stays live: the
#                      chain keeps advancing, the new blocks are self-built
#                      (builder_index == BUILDER_INDEX_SELF_BUILD), and CB returns
#                      204 "no header available" and NEVER 500.
#   --assert trusted-payment  the settings file sets max_execution_payment to the
#                      largest Uint64, the lodestar VC starts with
#                      --allowDangerousTrustedPayments, and buildoor claims half of
#                      each bid as execution_payment. HARD-fails unless every key
#                      loaded the cap, the BN selects CB's execution-payment bid in
#                      >= MIN_BUILDER_SLOTS slots, and for each of them values the
#                      bid at value + execution_payment. First checks, in a
#                      container with no beacon node, that the VC refuses to start
#                      on the same file without the flag (skip with
#                      TRUSTED_PAYMENT_CONTROL=0).
#   --assert relays-only  drop every [[mux]] from the CB config, so every key
#                      reaches buildoor through [[relays]]
#   --assert file-config  the VC loads FILE_CONFIG, a client's settings file from
#                      the docs, with the docs' example values swapped for the
#                      sim's. The entry the file reaches directly goes to
#                      FILE_CONFIG_DIRECT_URL, a closed port by default, so a
#                      builder bid can only have come through CB.
#   --assert manual-config  the keymanager API instead of a settings file: run a
#                      CB commit (CB_COMMIT) and write each key's builder_config
#                      with the curl call in that commit's
#                      docs/docs/get_started/epbs.md, run as written with the
#                      sim's CB URL and buildoor's hostname in place of the page's
#                      example values. Then the default assertion.
#   --assert dial      the settings file's auth_data names buildoor by URL, which
#                      no relay entry names (the relay entry names buildoor by IP),
#                      so CB dials it. Needs a CB_COMMIT built with
#                      scripts/cb-dial-test.patch, since devnet addresses are
#                      private.
# No flag = the default builder-built assertion.
#
# --render-only DIR renders the mode's CB config and the VC's settings file into
# DIR through the same code path as a live run, and exits: no docker, no kurtosis.
#
# One devnet at a time (~15G). Usage:
#   scripts/run-epbs-sim.sh [--assert p2p|block-submission|builder-down|trusted-payment|manual-config|dial|relays-only|file-config] [--render-only DIR]
# (or `just epbs-sim` / `just epbs-sim-assert <mode>`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ---- knobs (env-overridable) ------------------------------------------------
ENCLAVE="${ENCLAVE:-epbs-sim}"
# Upstream ethereum-package (kurtosis fetches + caches it). The repo's pinned
# submodule predates gloas-genesis compatibility with the local/lodestar:km image
# (see docs/EPBS.md), so this harness uses upstream.
# The default is pinned to a COMMIT: a bare github.com/... reference resolves to
# upstream HEAD at launch time, so the same harness can stand up a different
# devnet on every run and neither a PASS nor a regression can be reproduced or
# bisected. Override to try a newer upstream
# (EP_PACKAGE=github.com/ethpandaops/ethereum-package@<ref>) or to point at a
# local checkout ($REPO_ROOT/ethereum-package) once the submodule is upgraded.
EP_PACKAGE="${EP_PACKAGE:-github.com/ethpandaops/ethereum-package@6dd3f2613d1f9d1a9274864083c692726146c9db}"
ARGS_FILE="${ARGS_FILE:-configs/epbs/gloas-epbs.yaml}"
# PRESET override for network_params.preset in the args file (minimal|mainnet).
# Empty = use the file's value as-is. minimal = fast (8-slot epochs, buildoor
# activates ~slot 33) and is enough for every mode: CB forwards the revealed block
# unparsed, so its compiled preset no longer has to match the chain's. mainnet =
# 32-slot epochs (buildoor activates ~slot 128) and a ~30-45 min run.
PRESET="${PRESET:-}"
# Empty = provision from the pinned commit-boost-client submodule (sha-tagged,
# never stale; see scripts/ensure-cb-artifacts.sh). Set explicitly to pin a
# different CB image.
CB_IMAGE="${CB_IMAGE:-}"
# Build the CB image from this commit instead of the submodule's checkout; it must
# be in the submodule's repo (scripts/ensure-cb-artifacts.sh).
# manual-config also reads the docs page and checks the config keys at it.
# Empty = the submodule's HEAD.
export CB_COMMIT="${CB_COMMIT:-}"
CB_SUB="$REPO_ROOT/commit-boost-client"
# Swap the CL/VC image to test a specific consensus client build against the sim
# (e.g. CL_IMAGE=chainsafe/lodestar:v1.47.0-rc.0). Empty = use the args file's.
CL_IMAGE="${CL_IMAGE:-}"
# Kurtosis --image-download: "missing" uses a locally built image instead of pulling
IMAGE_DOWNLOAD="${IMAGE_DOWNLOAD:-always}"
CB_NAME="${CB_NAME:-cb-epbs}"            # must match advertised host in the templates
# How commit-boost joins the devnet:
#   service (default) = a first-class kurtosis enclave service (proper enclave DNS,
#                       one `kurtosis enclave rm` teardown, shows in `enclave inspect`).
#   docker            = legacy raw `docker run` on the enclave network (fallback).
CB_LAUNCH="${CB_LAUNCH:-service}"
CB_ARTIFACT="${CB_ARTIFACT:-cb-epbs-config}"  # kurtosis files-artifact name (service path)
# The relay CB bids into, rendered into BOTH __BUILDOOR_RELAY_URL__ sites in
# cb-config.toml.tmpl: the top-level [[relays]] entry and the [[mux.relays]]
# entry the listed validator pubkeys route to. Substituting only one leaves the
# validators bidding to the other relay while the file reads as if retargeted.
# The URL embeds the relay's BLS pubkey, so pointing at a different relay means
# supplying its pubkey too. Default = the devnet's buildoor.
BUILDOOR_RELAY_URL="${BUILDOOR_RELAY_URL:-http://0x8de7ec501d574152f52a962bf588573df2fc3563fd0c6077651208ed20f24f3d8572425706b343117b48bdca56808416@buildoor:8080}"
BUILDOOR_ACTIVATION_TIMEOUT="${BUILDOOR_ACTIVATION_TIMEOUT:-1200}"  # s to wait for the builder deposit to activate (queue delay varies run to run, seen out to ~slot 100)
OBSERVE_SLOTS="${OBSERVE_SLOTS:-16}"     # slots to watch once buildoor is active
MIN_BUILDER_SLOTS="${MIN_BUILDER_SLOTS:-8}"  # PASS threshold (builder-built via CB; allows some missed slots)
KEEP="${KEEP:-0}"                        # 1 = leave the enclave + CB running
RUN_DIR="configs/epbs/.run"              # rendered (gitignored) config lives here
# Extra params for participant 1's VC, space-separated, rendered into a copy of the
# args file (e.g. VC_EXTRA_PARAMS=--builder.selection=maxprofit on lodestar).
VC_EXTRA_PARAMS="${VC_EXTRA_PARAMS:-}"
# 0 = do not wait for buildoor's first bid via CB before observing: for a client
# whose beacon node never requests bids
WAIT_BUILDOOR="${WAIT_BUILDOOR:-1}"
# observe from this head slot at the earliest (e.g. past a gloas fork at epoch 1)
OBSERVE_FROM_SLOT="${OBSERVE_FROM_SLOT:-0}"
# opt-in assertion mode (default = today's builder-built assertion)
ASSERT_MODE="${ASSERT_MODE:-}"
RENDER_ONLY=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --assert)   ASSERT_MODE="${2:-}"; shift 2 ;;
    --assert=*) ASSERT_MODE="${1#*=}"; shift ;;
    --render-only)   RENDER_ONLY="${2:-}"; shift 2 ;;
    --render-only=*) RENDER_ONLY="${1#*=}"; shift ;;
    -h|--help)  sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done
no_mode() { printf -- '--assert %s is not a mode: %s\n' "$ASSERT_MODE" "$1" >&2; exit 2; }
case "$ASSERT_MODE" in
  ""|p2p|block-submission|builder-down|trusted-payment|manual-config|dial|relays-only|file-config) : ;;
  preserve|config-matrix|detect) no_mode "retired with builder-config, which is parked: the VC reads a proposer settings file" ;;
  ladder) no_mode "retired, CB plays no timing games on the ePBS bid path" ;;
  request-auth) no_mode "retired, CB no longer verifies the request auth (verify_builder_request_auth is gone); the builder does" ;;
  direct) no_mode "retired, builder-config writes no direct builder entries (the overlay refuses direct_entries); with CB down the beacon node falls back to p2p bids and local blocks" ;;
  pipe) no_mode "renamed: --assert dial" ;;
  *) printf 'unknown --assert mode: %s (want p2p|block-submission|builder-down|trusted-payment|manual-config|dial|relays-only|file-config)\n' "$ASSERT_MODE" >&2; exit 2 ;;
esac

# mainnet has 32-slot epochs, so buildoor activates ~slot 128 not ~33; give the
# activation poll room unless the caller already raised it above the default.
if [[ "$PRESET" == "mainnet" && "$BUILDOOR_ACTIVATION_TIMEOUT" == "1200" ]]; then
  BUILDOOR_ACTIVATION_TIMEOUT=2400
fi
case "$PRESET" in
  ""|minimal|mainnet) : ;;
  *) printf 'unknown PRESET: %s (want minimal|mainnet)\n' "$PRESET" >&2; exit 2 ;;
esac

# builder-down runs a second observe window after stopping buildoor
BUILDER_DOWN_SLOTS="${BUILDER_DOWN_SLOTS:-12}"  # slots to watch after buildoor is stopped
# trusted-payment: the settings file's cap, the largest Uint64, so the BN counts the
# whole execution payment
UNCLAMPED_CAP=18446744073709551615
# trusted-payment: 1 = first check the VC refuses the file without the flag
TRUSTED_PAYMENT_CONTROL="${TRUSTED_PAYMENT_CONTROL:-1}"

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
# CB colors its stdout by default ([logs.stdout] color), which puts escape codes
# between a field's name and its value, so the ANSI is stripped here: otherwise
# a grep for relay_id="buildoor-mux" never matches and the activation wait
# times out on a CB that is serving bids.
cb_logs() {
  if [[ "$CB_LAUNCH" == "service" ]]; then
    kurtosis service logs -a "$ENCLAVE" "$CB_NAME" 2>&1 || true
  else
    docker logs "$CB_NAME" 2>&1 || true
  fi | sed -r 's/\x1b\[[0-9;]*m//g'
}

# Beacon node logs: bid selection (the min_bid p2p floor + the winning bidSource)
# is logged by the BN's produceBlockV4 path, not the VC. cl_log_level=debug in the
# args file surfaces the debug-level "Ignoring p2p bid below min bid" line. `-a`
# for the same reason as cb_logs: the rejection line lives in the full history,
# not the 200-line tail (the tail hiding it was the sole cause of the earlier
# "floor unexercised" flake).
bn_logs() { kurtosis service logs -a "$ENCLAVE" "$BN_SVC" 2>&1 || true; }

# ---- keymanager helpers -------------------------------------------------------
# Authenticated GET of a validator's builder_config, against the exposed VC
# keymanager port + ethereum-package's static token (see docs/EPBS.md, "How the
# keymanager calls happen"). KM_TOKEN/VC_KM are set before use.
km_get()  { curl -sf -H "Authorization: Bearer $KM_TOKEN" "$VC_KM/eth/v1/validator/$1/builder_config"; }
# the entry URLs stored on a key (one per line)
km_entry_urls() {
  km_get "$1" | python3 -c \
    "import sys,json;d=json.load(sys.stdin).get('data',{});[print(e.get('url','')) for e in (d.get('builders') or [])]" \
    2>/dev/null || true
}

# head slot as a plain integer (0 on any error)
bn_head_slot() {
  curl -sf "$BN/eth/v1/beacon/headers/head" \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0
}

# Classify each block in [s0,s1]: prints "builder=<slots>", "self=<slots>" and
# "payment_only=<slots>" (comma-separated). A gloas block is builder-built when
# its signed_execution_payload_bid names a registry builder, self-built when it
# carries BUILDER_INDEX_SELF_BUILD. The bid's value is NOT the marker: a builder
# paying through execution_payment bids value = 0 (payment_only lists those).
# Unclassifiable slots are reported on stderr and land in no list; see
# scripts/epbs-blocks.py.
classify_blocks() { # $1=s0 $2=s1
  python3 "$REPO_ROOT/scripts/epbs-blocks.py" classify "$BN" "$1" "$2"
}

# number of entries in a comma-separated slot list ("" -> 0)
count_csv() { if [[ -n "$1" ]]; then awk -F, '{print NF}' <<<"$1"; else echo 0; fi; }

# --- assert block-submission: CB forwards the reveal unparsed -----------------
# The proposer reveals every builder-built block by POSTing the gloas
# SignedBeaconBlock to its builder URL (CB) at POST /eth/v1/builder/beacon_blocks.
# CB decodes nothing: it checks the request (a gloas Eth-Consensus-Version, an SSZ
# body), forwards the bytes to every configured relay and answers the BN 202 once
# forwarded, whatever the relays answer:
#   4xx -> refused before the forward: 400 (no gloas Eth-Consensus-Version) or
#          415 (not SSZ). A regression on either side of the hop.
#   202 -> forwarded. CB's own log says how many builders accepted it
#          ("signed beacon block forwarded" with accepted=<n>; an older CB logged
#          "signed beacon block accepted" or "no builder accepted"). None is
#          expected while buildoor gets the block over p2p.
# A forward that got no answer at all (timeout, connection failure) is a
# "builder did not accept the block" line carrying a reqwest error.
# A CB from before the always-202 change answered 5xx when no relay accepted.
# PASS: >=1 reveal forwarded, zero 4xx, zero forwards without an answer.
assert_block_submission() {
  log "assert block-submission: CB forwards the CL's gloas SignedBeaconBlock unparsed at /beacon_blocks"
  local CB_LOG bb fwd_2xx refused fwd_5xx accepted none_accepted unanswered reached
  CB_LOG="$(cb_logs | sed -r 's/\x1b\[[0-9;]*m//g')"   # strip ANSI so the status parses
  bb="$(grep 'method=/eth/v1/builder/beacon_blocks' <<<"$CB_LOG" | grep -oE 'Responded with [0-9]{3}' || true)"
  fwd_2xx=$(grep -c 'Responded with 2' <<<"$bb" || true)
  refused=$(grep -c 'Responded with 4' <<<"$bb" || true)
  fwd_5xx=$(grep -c 'Responded with 5' <<<"$bb" || true)
  local fwd_lines
  fwd_lines="$(grep 'signed beacon block forwarded' <<<"$CB_LOG" || true)"
  accepted=$(( $(grep -c 'signed beacon block accepted' <<<"$CB_LOG" || true) + $(grep -cE 'accepted=[1-9]' <<<"$fwd_lines" || true) ))
  none_accepted=$(( $(grep -c 'no builder accepted the signed beacon block' <<<"$CB_LOG" || true) + $(grep -cE 'accepted=0([^0-9]|$)' <<<"$fwd_lines" || true) ))
  unanswered=$(grep 'builder did not accept the block' <<<"$CB_LOG" | grep -c 'reqwest error' || true)
  reached=$(( fwd_2xx + fwd_5xx ))
  echo "  reveals forwarded (2xx, or 5xx on an older CB):  $reached (need >= 1)"
  echo "  a builder accepted / none accepted (CB's log):  $accepted / $none_accepted"
  echo "  /beacon_blocks 4xx (refused before forward):    $refused (need 0)"
  echo "  forwards with no answer (reqwest error):        $unanswered (need 0)"
  if (( reached >= 1 && refused == 0 && unanswered == 0 )); then
    printf '\033[1;32mPASS: CB forwarded %s gloas SignedBeaconBlock(s) unparsed (builder accepted %s, none accepted %s, 0 refused, 0 unanswered)\033[0m\n' \
      "$reached" "$accepted" "$none_accepted"
    return 0
  fi
  echo "  --- CB /beacon_blocks lines (tail) ---"
  grep 'method=/eth/v1/builder/beacon_blocks' <<<"$CB_LOG" | tail -10 | sed 's/^/    /'
  die "block-submission failed (forwarded=$reached need>=1, 4xx=$refused need0, unanswered=$unanswered need0)"
}

# CB's URL as the VC calls it: the url of the settings file's builder entry
CB_ENTRY_URL="http://${CB_NAME}:18550"

# --- assert builder-down: builder failure never stalls the proposer -------------
# buildoor is stopped after >=1 builder-built slot. The proposer must fall back to
# self-building: the chain keeps advancing, the new blocks carry
# BUILDER_INDEX_SELF_BUILD (a local block), and CB returns 204 "no
# header available" (relay unreachable -> no bid) and never a 500. Hard-fail on a
# stalled chain or any 5xx from the bid endpoint.
assert_builder_down() { # $1=slot at which buildoor was already confirmed building
  log "assert builder-down: stop buildoor, prove the proposer stays live"
  # discover the buildoor service name in the enclave (config DNS is 'buildoor')
  local BUILDOOR_SVC
  BUILDOOR_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE '^[[:space:]]*[0-9a-f]+[[:space:]]+buildoor[^[:space:]]*' | awk '{print $2}' | head -1)"
  [[ -z "$BUILDOOR_SVC" ]] && BUILDOOR_SVC="buildoor"
  echo "  buildoor service = $BUILDOOR_SVC"

  local stop_slot; stop_slot="$(bn_head_slot)"
  local cb_watermark; cb_watermark="$(cb_logs | wc -l)"
  log "stopping buildoor at head slot $stop_slot (CB log watermark $cb_watermark lines)"
  kurtosis service stop "$ENCLAVE" "$BUILDOOR_SVC" >/dev/null 2>&1 \
    || die "could not stop buildoor service '$BUILDOOR_SVC'"
  sleep 2
  kurtosis service inspect "$ENCLAVE" "$BUILDOOR_SVC" 2>/dev/null | grep -qiE 'STOPPED|Status.*stop' \
    && echo "  buildoor stopped" || echo "  buildoor stop issued (status not confirmed via inspect)"

  local target=$(( stop_slot + BUILDER_DOWN_SLOTS ))
  log "observing slots ${stop_slot}..${target} with buildoor DOWN ($(( BUILDER_DOWN_SLOTS * 6 ))s)"
  local cur=0
  for i in $(seq 1 $(( BUILDER_DOWN_SLOTS + 4 )) ); do
    cur="$(bn_head_slot)"
    printf '  head=%s / target=%s (builder down)\r' "$cur" "$target"
    (( cur >= target )) && break
    sleep 6
  done
  echo

  # 1) chain advanced (proposer did not stall)
  local advanced=$(( cur - stop_slot ))
  echo "  head advanced $advanced slots after buildoor stop (target window $BUILDER_DOWN_SLOTS)"

  # 2) classify blocks strictly AFTER the stop settled (stop_slot+2 onward): they
  #    must be self-built (BUILDER_INDEX_SELF_BUILD); a builder-built block with
  #    buildoor down is unexpected.
  local from=$(( stop_slot + 2 ))
  local cls; cls="$(classify_blocks "$from" "$cur")"
  local built_after selfb_after
  built_after="$(sed -n 's/^builder=//p' <<<"$cls")"
  selfb_after="$(sed -n 's/^self=//p' <<<"$cls")"
  local n_built_after n_self_after
  n_built_after=$( [[ -n "$built_after" ]] && awk -F, '{print NF}' <<<"$built_after" || echo 0 )
  n_self_after=$( [[ -n "$selfb_after" ]] && awk -F, '{print NF}' <<<"$selfb_after" || echo 0 )
  echo "  self-built blocks after stop:             $n_self_after  [slots: $selfb_after]"
  echo "  builder-built blocks after stop:          $n_built_after  [slots: $built_after]"

  # 3) CB bid endpoint: 204 "no header available", never a 500, in the post-stop log
  local CB_TAIL; CB_TAIL="$(cb_logs | tail -n +$(( cb_watermark + 1 )))"
  local cb_204 cb_500
  cb_204=$(grep -cE 'no header available for slot|no bid for slot' <<<"$CB_TAIL" || true)
  cb_500=$(grep 'get_execution_payload_bid failed' <<<"$CB_TAIL" | grep -c -E 'Internal|internal server error' || true)
  echo "  CB 204 'no header available' after stop:   $cb_204"
  echo "  CB 500 (Internal) after stop:              $cb_500"

  log "RESULT"
  # advance tolerance: allow up to 3 missed slots across the window
  if (( advanced >= BUILDER_DOWN_SLOTS - 3 && n_self_after >= 1 && n_built_after == 0 && cb_500 == 0 && cb_204 >= 1 )); then
    printf '\033[1;32mPASS: buildoor down -> chain advanced %s slots, %s self-built blocks, CB 204x%s and 0 500s\033[0m\n' \
      "$advanced" "$n_self_after" "$cb_204"
    return 0
  fi
  cb_logs | tail -n +$(( cb_watermark + 1 )) | grep -E 'no header|get_execution_payload_bid failed|Internal' | tail -20
  (( advanced >= BUILDER_DOWN_SLOTS - 3 )) || die "builder-down: chain STALLED (advanced $advanced, need >= $(( BUILDER_DOWN_SLOTS - 3 )))"
  (( cb_500 == 0 )) || die "builder-down: CB returned $cb_500 x 500 with buildoor down (must be 204)"
  (( n_built_after == 0 )) || die "builder-down: $n_built_after builder-built block(s) appeared with buildoor DOWN"
  die "builder-down: expected self-built blocks (self=$n_self_after) and >=1 CB 204 (204=$cb_204)"
}

# --- assert trusted-payment: the BN selects CB's bid and counts its whole payment --
# CB ranks nothing: it forwards buildoor's bid as received. The BN values a bid at
# value + min(execution_payment, cap) with the cap stored on the builder_config
# entry (beacon-APIs #630), and "unclamped" stores the largest Uint64, so the
# whole execution payment counts. For each slot in the window where the BN
# selected CB's bid ("Selected builder block ... bidSource=<CB>"), the bid CB
# received from buildoor-mux ("received new header", debug) must be that bid
# (same value and execution_payment) and the BN's total for the CB candidate
# ("Ranked builder bid candidates ... <CB>:total=N", debug) must be
# value + execution_payment. PASS: >= MIN_BUILDER_SLOTS CB selections carry
# execution_payment > 0 and no selected slot mismatches or lacks either side.
assert_trusted_payment() { # $1,$2 = observed window
  log "assert trusted-payment: the BN selects CB's execution-payment bid and counts the whole payment"
  # The window ends as its last slot's bid is served, and CB's log stream can lag
  # a few seconds, so wait (up to 15 s) for CB's bid line for that slot
  local tries=0
  until cb_logs | sed -r 's/\x1b\[[0-9;]*m//g' | grep -qE "received header.*slot=$2([^0-9]|$)" || (( ++tries >= 15 )); do
    sleep 1
  done
  cb_logs | sed -r 's/\x1b\[[0-9;]*m//g' > "$RUN_DIR/cb.log"
  bn_logs | sed -r 's/\x1b\[[0-9;]*m//g' > "$RUN_DIR/bn.log"
  local report rc=0
  report="$(python3 - "$RUN_DIR/cb.log" "$RUN_DIR/bn.log" "$1" "$2" "$CB_ENTRY_URL" \
      "$UNCLAMPED_CAP" "$MIN_BUILDER_SLOTS" <<'PY'
import re,sys
from decimal import Decimal
cb_log,bn_log=sys.argv[1],sys.argv[2]
s0,s1,cb_url,cap,need=int(sys.argv[3]),int(sys.argv[4]),sys.argv[5],int(sys.argv[6]),int(sys.argv[7])
def gwei(eth): return int(Decimal(eth)*10**9)
cb={}      # slot -> {(value, execution_payment)} of the bids buildoor gave CB, gwei
for line in open(cb_log,errors="replace"):
    # A CB from before #505's review logs "received new header" with the relay id;
    # later ones log "received header" for the bid they return (buildoor is the only relay)
    if not ("received new header" in line and 'relay_id="buildoor-mux"' in line) and "received header " not in line: continue
    slot=re.search(r"\bslot=(\d+)",line)
    v=re.search(r'trustless_bid_eth="([0-9.]+)"',line)
    ep=re.search(r'execution_payment_eth="([0-9.]+)"',line)
    if slot and v and ep: cb.setdefault(int(slot.group(1)),set()).add((gwei(v.group(1)),gwei(ep.group(1))))
ranked,selected={},{}
source=re.compile(r"bidSource="+re.escape(cb_url)+r"(?=[,\s]|$)")
for line in open(bn_log,errors="replace"):
    m=re.search(r"\bslot=(\d+)",line)
    if not m or not s0<=int(m.group(1))<=s1: continue
    slot=int(m.group(1))
    if "Ranked builder bid candidates" in line:
        t=re.search(re.escape(cb_url)+r":total=(\d+)",line)
        if t: ranked[slot]=int(t.group(1))
    elif "Selected builder block" in line and source.search(line):
        v=re.search(r"\bbidValue=(\d+)",line)
        ep=re.search(r"\bbidExecutionPayment=(\d+)",line)
        if v and ep: selected[slot]=(int(v.group(1)),int(ep.group(1)))
with_ep=[s for s in selected if selected[s][1]>0]
problems,agree=[],[]
for slot in sorted(selected):
    if slot not in cb:
        problems.append("slot {0}: the BN selected CB's bid but CB logged no bid from buildoor-mux".format(slot)); continue
    if selected[slot] not in cb[slot]:
        problems.append("slot {0}: the BN's selected value/execution_payment {1} is not a bid CB received {2}".format(slot,selected[slot],sorted(cb[slot]))); continue
    v,ep=selected[slot]
    want=v+min(ep,cap)
    if slot not in ranked:
        problems.append("slot {0}: no 'Ranked builder bid candidates' total for {1}".format(slot,cb_url)); continue
    if ranked[slot]!=want:
        problems.append("slot {0}: BN valuation {1} != value + execution_payment {2}".format(slot,ranked[slot],want)); continue
    agree.append(slot)
print("  BN selected CB's bid in [{0},{1}]:           {2} slots".format(s0,s1,len(selected)))
print("    with execution_payment > 0:               {0} (need >= {1})".format(len(with_ep),need))
print("  BN valuation == value + execution_payment:  {0} of {1}".format(len(agree),len(selected)))
if agree:
    s=agree[0]; v,ep=selected[s]
    print("    e.g. slot {0}: value={1} execution_payment={2} -> BN total={3}".format(s,v,ep,ranked[s]))
for p in problems[:8]: print("    "+p)
ok=len(with_ep)>=need and not problems
print("the BN selected CB's execution-payment bid in {0} slots and counted the whole payment in {1} of {2}".format(
    len(with_ep),len(agree),len(selected)))
sys.exit(0 if ok else 1)
PY
)" || rc=$?
  sed '$d' <<<"$report"
  (( rc == 0 )) || die "trusted-payment: $(tail -1 <<<"$report")"
  printf '\033[1;32mPASS: %s\033[0m\n' "$(tail -1 <<<"$report")"
}

# --- manual-config: builder config written the way the CB docs page says -------
# The CB docs page documents the keymanager call an operator can make instead of a
# settings file. This mode makes that call, taken from the page at the commit
# under test, so a documented call that does not route the proposer's bid
# requests to CB fails the run.
DOCS_PAGE="docs/docs/get_started/epbs.md"
# the page's example values, swapped for the sim's before the call runs
DOCS_CB_URL="http://cb.example.com:18550"
DOCS_AUTH_DATA="0x6275696c6465722d612e6578616d706c652e636f6d"   # builder-a.example.com

# --- file-config: a client's builder config file, loaded by the VC -------------
FILE_CONFIG="${FILE_CONFIG:-}"
FILE_CONFIG_DIRECT_URL="${FILE_CONFIG_DIRECT_URL:-http://127.0.0.1:1}"
FILE_CONFIG_OUT="${FILE_CONFIG_OUT:-}"           # where the rendered file is kept
DOCS_DIRECT_URL="https://builder-a.example.com"
DOCS_AUTH_DATA_B64="YnVpbGRlci1hLmV4YW1wbGUuY29t"   # builder-a.example.com, as Prysm writes bytes
FC_MOUNT="/cb-file-config"

# FILE_CONFIG with the example values swapped: CB's URL, buildoor's auth data in
# hex and base64, and the directly reached entry
render_file_config() { # $1=out path
  [[ -f "$FILE_CONFIG" ]] || die "file-config: FILE_CONFIG=${FILE_CONFIG:-<unset>} is not a file"
  local host hex b64
  host="$(sed -E 's|^[a-z]+://([^@/]*@)?([^:/]+).*$|\2|' <<<"$BUILDOOR_RELAY_URL")"
  hex="0x$(printf '%s' "$host" | xxd -p | tr -d '\n')"
  b64="$(printf '%s' "$host" | base64 -w0)"
  sed -e "s|$DOCS_CB_URL|$CB_ENTRY_URL|g" -e "s|$DOCS_DIRECT_URL|$FILE_CONFIG_DIRECT_URL|g" \
    -e "s|$DOCS_AUTH_DATA|$hex|g" -e "s|$DOCS_AUTH_DATA_B64|$b64|g" "$FILE_CONFIG" > "$1"
  # comments may name the example host; only values must not
  if grep -v '^[[:space:]]*#' "$1" | grep -q 'example\.com\|YnVpbGRlci1h'; then
    die "file-config: $FILE_CONFIG keeps an example value the harness does not swap: $(grep -v '^[[:space:]]*#' "$1" | grep -m1 'example\.com\|YnVpbGRlci1h')"
  fi
}

# Lighthouse reads builder_definitions.yml from its validators dir at startup, a
# keystore mount, so the file goes in after launch and the VC restarts
install_lighthouse_file_config() { # $1=rendered file
  local ctr dir
  ctr="$(docker ps --format '{{.Names}}' | grep -m1 "^${VC_SVC}--")" || die "file-config: no container for $VC_SVC"
  dir="$(docker inspect -f '{{range .Args}}{{println .}}{{end}}{{range .Config.Cmd}}{{println .}}{{end}}' "$ctr" \
    | grep -oE -- '--validators-dir=[^ ]+' | head -1 | cut -d= -f2)"
  [[ -n "$dir" ]] || die "file-config: cannot find --validators-dir in $ctr's command"
  docker cp "$1" "$ctr:$dir/builder_definitions.yml" || die "file-config: docker cp into $ctr:$dir failed"
  docker restart "$ctr" >/dev/null || die "file-config: restarting $ctr failed"
  echo "file-config: $dir/builder_definitions.yml installed, $ctr restarted"
}

# ---- the VC's proposer settings file ------------------------------------------
# Every mode but manual-config writes one: a single builder entry, CB, whose
# auth_data names buildoor's host (dial: buildoor's URL). min_bid floors
# buildoor's 0.101 ETH p2p bid and passes its 0.2 ETH builder-API bid. Prysm's
# file carries the VC's own fee recipient (ethereum-package's default), since the
# beacon node discards a bid whose fee recipient differs from the proposer's
# preferences.
SETTINGS_MIN_BID_GWEI=150000000
SETTINGS_FEE_RECIPIENT="0x8943545177806ED17B9F23F0a21ee5948eCaa776"

write_settings_file() { # $1=cl_type $2=out path
  local host auth cap=0
  host="$(sed -E 's|^[a-z]+://([^@/]*@)?([^:/]+).*$|\2|' <<<"$BUILDOOR_RELAY_URL")"
  auth="$host"; [[ "$ASSERT_MODE" == "dial" ]] && auth="$DIAL_AUTH_URL"
  [[ "$ASSERT_MODE" == "trusted-payment" ]] && cap="$UNCLAMPED_CAP"
  python3 - "$1" "$2" "$CB_ENTRY_URL" "0x$(printf '%s' "$auth" | xxd -p | tr -d '\n')" \
    "$SETTINGS_MIN_BID_GWEI" "$cap" "$SETTINGS_FEE_RECIPIENT" <<'PY'
import json, sys
cl, out, url, auth, min_bid, cap, fee = sys.argv[1:]
if cl == "lodestar":
    text = ("default_config:\n  builder:\n"
            f'    min_bid: "{min_bid}"\n    max_execution_payment: "{cap}"\n'
            f'    builders:\n      - url: "{url}"\n        auth_data: "{auth}"\n')
elif cl == "prysm":
    text = json.dumps({"version": 2, "default_config": {"fee_recipient": fee, "builder": {
        "min_bid": min_bid, "max_execution_payment": cap,
        "builders": [{"url": url, "auth_data": auth}]}}}, indent=2) + "\n"
else:
    text = (f"min_bid: {min_bid}\nbuilders:\n  - enabled: true\n"
            f'    url: "{url}"\n    auth_data: "{auth}"\n    max_execution_payment: {cap}\n')
open(out, "w").write(text)
PY
}

# Writes the run's settings file into $1 and sets SETTINGS_CL and SETTINGS_FILE:
# file-config's FILE_CONFIG with the sim's values, otherwise the harness's own
render_settings_file() { # $1=out dir
  SETTINGS_CL="$(grep -m1 -oE 'cl_type: *[a-z]+' "$ARGS_FILE" | awk '{print $2}')"
  case "$SETTINGS_CL" in
    lodestar) SETTINGS_FILE="$1/proposer-settings.yaml" ;;
    prysm) SETTINGS_FILE="$1/proposer-settings.json" ;;
    lighthouse) SETTINGS_FILE="$1/builder_definitions.yml" ;;
    *) die "no proposer settings file for cl_type ${SETTINGS_CL:-<none>}; --assert manual-config writes builder config through the keymanager API" ;;
  esac
  mkdir -p "$1"
  if [[ "$ASSERT_MODE" == "file-config" ]]; then
    SETTINGS_FILE="$1/$(basename "$FILE_CONFIG")"
    render_file_config "$SETTINGS_FILE"
  else
    write_settings_file "$SETTINGS_CL" "$SETTINGS_FILE"
  fi
}

# --- trusted-payment, control: the VC refuses the file's cap without the flag ---
# Lodestar checks the settings file before it loads a keystore or calls a beacon
# node, so this needs no devnet: the file alone must stop the VC with an error
# naming the flag, and with the flag the VC must get past that check (it then
# stops for want of a keystore).
assert_trusted_payment_refused() { # $1=VC image $2=settings file
  log "trusted-payment control: the VC refuses the file's cap without --allowDangerousTrustedPayments"
  local image=$1 file=$2 dir out_without out_with rc=0
  dir="$(cd "$(dirname "$file")" && pwd)"
  vc_once() { # $@ = extra VC flags
    timeout 60 docker run --rm --network none -v "$dir:/f:ro" "$image" validator --network sepolia --dataDir /tmp/vc \
      --beaconNodes http://127.0.0.1:1 --proposerSettingsFile "/f/$(basename "$file")" "$@" 2>&1
  }
  out_without="$(vc_once)" || rc=$?
  out_with="$(vc_once --allowDangerousTrustedPayments)" || true
  local refused=0 passed=0
  grep -q 'requires --allowDangerousTrustedPayments' <<<"$out_without" && (( rc != 0 )) && refused=1
  grep -q 'requires --allowDangerousTrustedPayments' <<<"$out_with" || passed=1
  echo "  without the flag: exit $rc, $(grep -m1 -oE '[^ ]*Configuring a builder max execution payment[^"]*' <<<"$out_without" || echo "no refusal")"
  echo "  with the flag:    $(tail -1 <<<"$out_with" | cut -c1-160)"
  (( refused == 1 && passed == 1 )) || die "control: want a refusal naming the flag without it (refused=$refused) and none with it (passed=$passed)"
  printf '\033[1;32mPASS: the VC refuses the settings file'"'"'s cap without --allowDangerousTrustedPayments and accepts it with the flag\033[0m\n'
}

# --- trusted-payment: every key loaded the file's cap ---------------------------
assert_cap_loaded() {
  log "assert trusted-payment: every key's builder config holds max_execution_payment = $UNCLAMPED_CAP"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  python3 - "$VC_KM" "$KM_TOKEN" "$UNCLAMPED_CAP" <<'PY' || die "trusted-payment: a key's builder config lacks the file's cap"
import json,sys,urllib.request
km,token,cap=sys.argv[1],sys.argv[2],sys.argv[3]
def get(path):
    req=urllib.request.Request(km+path,headers={"Authorization":"Bearer "+token})
    with urllib.request.urlopen(req,timeout=10) as r:
        return json.load(r)
keys=[k["validating_pubkey"] for k in get("/eth/v1/keystores")["data"]]
entries,bad,sample=0,[],None
for k in keys:
    doc=get("/eth/v1/validator/{0}/builder_config".format(k))["data"]
    sample=sample or (k,doc)
    builders=doc.get("builders") or []
    if not builders:
        bad.append((k,"no builder entries"))
    for e in builders:
        entries+=1
        if str(e.get("max_execution_payment"))!=cap:
            bad.append((k,"{0}: max_execution_payment={1}".format(e.get("url"),e.get("max_execution_payment"))))
print("  keys: {0}, entries: {1}, entries without the cap: {2}".format(len(keys),entries,len(bad)))
if sample:
    print("    e.g. {0}: {1}".format(sample[0][:20]+"...",json.dumps(sample[1])))
for k,why in bad[:5]:
    print("    {0}: {1}".format(k,why))
sys.exit(1 if bad or not keys else 0)
PY
}

# the commit CB is built from: CB_COMMIT, else the submodule's checkout
cb_commit() {
  git -C "$CB_SUB" rev-parse --verify --quiet "${CB_COMMIT:-HEAD}^{commit}" \
    || die "CB_COMMIT=$CB_COMMIT is not a commit in $CB_SUB (fetch it: git -C commit-boost-client fetch <repo> <branch>)"
}

# The first bash block under the page's builder config heading (anchor
# validator-builder-config; titled "Validator builder config" before PR 1's docs
# rewrite and "Point each validator key at Commit-Boost" after it), with
# the sim's CB URL and buildoor's auth data in place of the example values. Fails
# when the page lacks the call or the example values, so a docs change cannot
# quietly turn the check into a call the page does not show.
docs_builder_config_call() { # $1=commit
  local page call host auth
  page="$(git -C "$CB_SUB" show "$1:$DOCS_PAGE")" || die "no $DOCS_PAGE at $1"
  call="$(awk '/^#+ /{s=($0 ~ /validator-builder-config|Validator builder config|Point each validator key at Commit-Boost/)} s && /^```bash$/{f=1; next} f && /^```$/{exit} f' <<<"$page")"
  # a curl call, or the token piped into one (`printf ... | curl ... -H @-`)
  [[ "$call" == *curl* && "$call" == *"/builder_config"* ]] \
    || die "$DOCS_PAGE at $1 has no curl call to builder_config under its builder config heading"
  [[ "$call" == *"$DOCS_CB_URL"* && "$call" == *"$DOCS_AUTH_DATA"* ]] \
    || die "the page's call no longer uses the example url $DOCS_CB_URL and auth_data $DOCS_AUTH_DATA; update DOCS_CB_URL/DOCS_AUTH_DATA"
  # the page's rule: the hex of the relay URL's hostname
  host="$(sed -E 's|^[a-z]+://([^@/]*@)?([^:/]+).*$|\2|' <<<"$BUILDOOR_RELAY_URL")"
  auth="0x$(printf '%s' "$host" | xxd -p | tr -d '\n')"
  call="${call//"$DOCS_CB_URL"/$CB_ENTRY_URL}"
  printf '%s\n' "${call//"$DOCS_AUTH_DATA"/$auth}"
}

# Every key the rendered CB config sets must be a name in the commit's
# crates/common/src, where Commit-Boost parses its config. CB ignores an unknown
# [pbs] or [[mux]] key, so a key it does not know would load as if it took effect.
check_config_keys() { # $1=cb-config.toml $2=commit
  local key unknown=()
  for key in $(sed 's/#.*$//' "$1" | grep -oE '(^|[{,])[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=' | grep -oE '[A-Za-z0-9_-]+' | sort -u); do
    git -C "$CB_SUB" grep -qw -e "$key" "$2" -- crates/common/src || unknown+=("$key")
  done
  (( ${#unknown[@]} == 0 )) || die "cb-config.toml sets keys CB @ ${2:0:12} does not know: ${unknown[*]}"
  echo "config keys: all known to CB @ ${2:0:12}"
}

# Each key the VC holds gets its builder config from the page's call, run as
# written; a curl wrapper only records the status code and response body.
# --- dial: every key's builder config names buildoor by URL, which no relay entry
# matches (the entry names buildoor by IP), so CB dials it. A devnet has only
# private addresses, which CB refuses to dial, so this needs a CB_COMMIT built with
# scripts/cb-dial-test.patch (docs/EPBS.md, dial mode). buildoor accepts the URL
# as auth data because it equals its --builder-api-url.
DIAL_AUTH_URL="${DIAL_AUTH_URL:-http://buildoor:8080}"
# PASS: CB dialed buildoor for the bids, never refused its address, never looped
assert_dial() {
  log "assert dial: CB dials the builder its relay entries do not name"
  local CB_LOG dialed refused looped
  CB_LOG="$(cb_logs | sed -r 's/\x1b\[[0-9;]*m//g')"
  dialed=$(grep 'outside the config, dialing it' <<<"$CB_LOG" | grep -cE "origin=${DIAL_AUTH_URL}( |$)" || true)
  refused=$(grep -c 'resolves to a disallowed address' <<<"$CB_LOG" || true)
  looped=$(grep -c 'request came from a Commit-Boost' <<<"$CB_LOG" || true)
  echo "  requests CB dialed $DIAL_AUTH_URL for:       $dialed (need >= 1)"
  echo "  dials refused as a disallowed address:     $refused (need 0: is CB built with cb-dial-test.patch?)"
  echo "  requests from a Commit-Boost refused:      $looped (need 0)"
  (( dialed >= 1 && refused == 0 && looped == 0 )) || die "dial failed (dialed=$dialed refused=$refused looped=$looped)"
  printf '\033[1;32mPASS: CB dialed %s for %s bid and preferences requests; the blocks above were built from those bids\033[0m\n' \
    "$DIAL_AUTH_URL" "$dialed"
}

write_documented_builder_configs() {
  log "manual-config: each key's builder_config via the call in $DOCS_PAGE @ ${CB_SHA:0:12}"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  local call="$RUN_DIR/builder-config-call.sh" resp="$RUN_DIR/builder-config-resp.txt" keys key code="" n=0
  docs_builder_config_call "$CB_SHA" > "$call"
  sed 's/^/    /' "$call"
  keys="$(curl -sf -H "Authorization: Bearer $KM_TOKEN" "$VC_KM/eth/v1/keystores" \
    | python3 -c 'import sys,json;[print(k["validating_pubkey"]) for k in json.load(sys.stdin)["data"]]')" \
    || die "manual-config: could not list the VC's keystores"
  [[ -n "$keys" ]] || die "manual-config: the VC holds no keys"
  for key in $keys; do
    code="$(KEYMANAGER_URL="$VC_KM" TOKEN="$KM_TOKEN" TOKEN_FILE="$RUN_DIR/km-token.txt" PUBKEY="$key" RESP="$resp" bash -c \
      'curl() { command curl -sS -o "$RESP" -w "%{http_code}" "$@"; }; . "$1"' _ "$call")" || true
    [[ "$code" == 2* ]] || die "manual-config: the documented call for $key got HTTP $code: $(head -c 300 "$resp" 2>/dev/null)"
    km_entry_urls "$key" | grep -qxF "$CB_ENTRY_URL" \
      || die "manual-config: $key stores no $CB_ENTRY_URL entry after HTTP $code"
    n=$(( n + 1 ))
  done
  echo "  $n keys written (HTTP $code), e.g. $key: $(km_get "$key")"
}

# ---- CB config rendering ------------------------------------------------------
# One code path for a live run, the pre-launch render and --render-only.
render_cb_config() { # $1=out dir $2=genesis_time
  local out="$1" gen_time="$2"
  # Strict-shape it before it reaches sed: it interpolates into a config the CB
  # process trusts, so a malformed BN response must fail loud, not inject.
  [[ "$gen_time" =~ ^[0-9]+$ ]] || die "genesis_time is not a plain integer: '$gen_time'"
  rm -rf "$out"; mkdir -p "$out"
  # km-token.txt is the well-known static ethereum-package keymanager token (public,
  # sim-only). It is NOT a secret and must never be reused against a real VC. See docs/EPBS.md.
  cp configs/epbs/km-token.txt "$out/km-token.txt"
  sed -e "s|__GENESIS_TIME__|$gen_time|" \
      -e "s|__BUILDOOR_RELAY_URL__|$BUILDOOR_RELAY_URL|g" \
      configs/epbs/cb-config.toml.tmpl > "$out/cb-config.toml"
  grep -q '__' "$out/cb-config.toml" && die "unrendered placeholder in cb-config.toml"

  # relays-only: no [[mux]], so every key reaches buildoor through [[relays]]
  if [[ "$ASSERT_MODE" == "relays-only" ]]; then
    python3 - "$out/cb-config.toml" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
start = re.search(r"^\[\[mux\]\]\s*$", s, re.M).start()
end = re.search(r"^\[logs\.stdout\]\s*$", s, re.M).start()
end = s.rfind("\n\n", start, end) + 2
open(p, "w").write(s[:start] + s[end:])
PY
    grep -q '^\[\[mux\]\]' "$out/cb-config.toml" && die "relays-only: a [[mux]] is left in cb-config.toml"
  fi
  return 0
}

# ---- render-only: one mode's CB config and settings file, no devnet ------------
if [[ -n "$RENDER_ONLY" ]]; then
  render_cb_config "$RENDER_ONLY" 0
  if [[ "$ASSERT_MODE" == "manual-config" ]]; then
    # no settings file: check the keys against the commit and print the call the run makes
    CB_SHA="$(cb_commit)"
    check_config_keys "$RENDER_ONLY/cb-config.toml" "$CB_SHA"
    docs_builder_config_call "$CB_SHA" > "$RENDER_ONLY/builder-config-call.sh"
    echo "rendered manual-config @ ${CB_SHA:0:12}: $RENDER_ONLY/cb-config.toml $RENDER_ONLY/builder-config-call.sh"
    sed 's/^/    /' "$RENDER_ONLY/builder-config-call.sh"
    exit 0
  fi
  render_settings_file "$RENDER_ONLY"
  echo "rendered ${ASSERT_MODE:-default}: $RENDER_ONLY/cb-config.toml $SETTINGS_FILE"
  sed 's/^/    /' "$SETTINGS_FILE"
  exit 0
fi

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
# Provision the CB image from the pinned submodule, sha-tagged so a stale build
# can never be silently used. Skipped when the caller pins CB_IMAGE explicitly.
if [[ -z "$CB_IMAGE" ]]; then
  # Capture stdout (the eval-able line); the script streams build output to
  # stderr live. Hard-fail on a build error rather than silently continuing with
  # an empty CB_IMAGE (the "silence == success" trap).
  cb_art="$(bash "$(dirname "$0")/ensure-cb-artifacts.sh" --no-bin)" \
    || die "provisioning the CB image from the submodule failed (see build output above)"
  eval "$cb_art"   # sets CB_IMAGE
fi
docker image inspect "$CB_IMAGE" >/dev/null 2>&1 || die "CB image $CB_IMAGE not present locally"
# Every builder-built count below rests on this classifier; check it against its
# boundary cases before spending a devnet on it.
python3 scripts/epbs-blocks.py self-test || die "scripts/epbs-blocks.py self-test failed"
# Render this mode's CB config with placeholder chain values, and the VC's settings
# file, now: a mistake in either would otherwise surface only once the devnet is up.
render_cb_config "$RUN_DIR" 0
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  CB_SHA="$(cb_commit)"
  check_config_keys "$RUN_DIR/cb-config.toml" "$CB_SHA"
  docs_builder_config_call "$CB_SHA" >/dev/null
  echo "docs:    $DOCS_PAGE @ ${CB_SHA:0:12}"
else
  # outside RUN_DIR, which the CB config render clears before the Lighthouse install
  render_settings_file "${FILE_CONFIG_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/epbs-settings.XXXXXX")}"
  echo "settings: $SETTINGS_FILE for $SETTINGS_CL"
  sed 's/^/    /' "$SETTINGS_FILE"
fi
avail_g=$(free -g | awk '/^Mem:/{print $7}')
(( avail_g >= 10 )) || echo "WARN: only ${avail_g}G RAM available; a devnet wants ~15G"
echo "CB img:  $CB_IMAGE"
echo "CB join: $CB_LAUNCH"
echo "assert:  ${ASSERT_MODE:-default (builder-built)}"
echo "enclave: $ENCLAVE   observe: ${OBSERVE_SLOTS} slots   pass>=${MIN_BUILDER_SLOTS}"

# PRESET override: render a copy of the args file with network_params.preset
# swapped, and launch from that. Leaves the committed args file untouched, so the
# preset is a pure runtime knob (minimal = fast default).
if [[ -n "$PRESET" ]]; then
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  sed -E "s/^([[:space:]]*preset:[[:space:]]*).*/\1$PRESET/" "$ARGS_FILE" > "$RENDERED_ARGS"
  grep -qE "^[[:space:]]*preset:[[:space:]]*$PRESET[[:space:]]*$" "$RENDERED_ARGS" \
    || die "PRESET render failed: no 'preset:' line to override in $ARGS_FILE"
  ARGS_FILE="$RENDERED_ARGS"
  echo "preset:  $PRESET (rendered -> $ARGS_FILE, activation timeout ${BUILDOOR_ACTIVATION_TIMEOUT}s)"
fi

# CL_IMAGE override: swap cl_image + vc_image to test a specific CL build. Same
# render-a-copy pattern as PRESET, so the committed args file stays untouched.
if [[ -n "$CL_IMAGE" ]]; then
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  sed -E "s#^([[:space:]]*(cl_image|vc_image):[[:space:]]*).*#\1$CL_IMAGE#" "$ARGS_FILE" > "$RENDERED_ARGS"
  grep -qE "^[[:space:]]*cl_image:[[:space:]]*$(printf '%s' "$CL_IMAGE" | sed 's/[.[\*^$]/\\&/g')[[:space:]]*$" "$RENDERED_ARGS" \
    || die "CL_IMAGE render failed: no 'cl_image:' line to override in $ARGS_FILE"
  ARGS_FILE="$RENDERED_ARGS"
  echo "cl/vc:   $CL_IMAGE (rendered -> $ARGS_FILE)"
fi

# trusted-payment: lodestar's VC refuses a settings file whose
# max_execution_payment is above 0 unless started with
# --allowDangerousTrustedPayments. Check that refusal first, on the VC image the
# run launches, then render a copy of the args file with the flag added to the
# first vc_extra_params list (participant 1, the VC that loads the file).
if [[ "$ASSERT_MODE" == "trusted-payment" ]]; then
  [[ "$SETTINGS_CL" == lodestar ]] || die "trusted-payment: participant 1 runs $SETTINGS_CL; --allowDangerousTrustedPayments is a lodestar VC flag"
  if [[ "$TRUSTED_PAYMENT_CONTROL" == "1" ]]; then
    assert_trusted_payment_refused "$(grep -m1 -oE 'vc_image: *[^ ]+' "$ARGS_FILE" | awk '{print $2}')" "$SETTINGS_FILE"
  fi
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  python3 - "$ARGS_FILE" "$RENDERED_ARGS" <<'PY' || die "trusted-payment: cannot use $ARGS_FILE (see above)"
import re,sys
src,dst=sys.argv[1],sys.argv[2]
flag="--allowDangerousTrustedPayments"
lines=open(src).read().splitlines(keepends=True)
if not any(flag in l for l in lines):
    i=next((i for i,l in enumerate(lines) if re.match(r"\s*vc_extra_params:\s*$",l)),None)
    item=re.match(r"(\s*)-\s",lines[i+1]) if i is not None and i+1<len(lines) else None
    if not item: sys.exit("no vc_extra_params list to add {0} to".format(flag))
    lines.insert(i+1,item.group(1)+"- "+flag+"\n")
    # buildoor pays everything through value unless told to claim part of the
    # bid as execution_payment
    pay="--builder-api-execution-payment-percent=50"
    b=next((i for i,l in enumerate(lines) if re.match(r"buildoor_params:\s*$",l)),None)
    e=next((i for i in range(b+1,len(lines)) if re.match(r"\s+extra_args:\s*$",lines[i])),None) if b is not None else None
    item=re.match(r"(\s*)-\s",lines[e+1]) if e is not None and e+1<len(lines) else None
    if not item: sys.exit("no buildoor_params.extra_args list to add {0} to".format(pay))
    lines.insert(e+1,item.group(1)+"- "+pay+"\n")
open(dst,"w").write("".join(lines))
PY
  ARGS_FILE="$RENDERED_ARGS"
  grep -q -- '- --allowDangerousTrustedPayments$' "$ARGS_FILE" || die "trusted-payment: flag render failed"
  grep -q -- '- --builder-api-execution-payment-percent=50$' "$ARGS_FILE" || die "trusted-payment: buildoor flag render failed"
  echo "vc flag: --allowDangerousTrustedPayments (rendered -> $ARGS_FILE)"
fi

# buildoor's builder-API bid is only its subsidy (0.0001 ETH by default), so its
# 0.101 ETH p2p bid would win, and the config's min_bid would floor CB's bid too.
# Raise the builder-API subsidy above both, so a block built from a bid CB
# returned shows the config routes the proposer's bid requests through CB.
BUILDER_API_SUBSIDY_GWEI=200000000
# the page's call or the docs' file may set no min_bid: a grep that matches nothing must not end the run
case "$ASSERT_MODE" in
  manual-config) config_min_bid="$(docs_builder_config_call "$CB_SHA" | grep -oE '"min_bid": *"[0-9]+"' | grep -oE '[0-9]+' | head -1 || true)" ;;
  file-config) config_min_bid="$(grep -oE 'min_bid"?: *"?[0-9]+' "$FILE_CONFIG" | grep -oE '[0-9]+$' | sort -n | tail -1 || true)" ;;
  *) config_min_bid="$SETTINGS_MIN_BID_GWEI" ;;
esac
(( ${config_min_bid:-0} < BUILDER_API_SUBSIDY_GWEI )) \
  || die "the config's min_bid $config_min_bid is not below the builder-API subsidy $BUILDER_API_SUBSIDY_GWEI"
RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
python3 - "$ARGS_FILE" "$RENDERED_ARGS" "--builder-api-subsidy=$BUILDER_API_SUBSIDY_GWEI" <<'PY' || die "cannot add buildoor's subsidy to $ARGS_FILE (see above)"
import re,sys
src,dst,flag=sys.argv[1],sys.argv[2],sys.argv[3]
lines=open(src).read().splitlines(keepends=True)
b=next((i for i,l in enumerate(lines) if re.match(r"buildoor_params:\s*$",l)),None)
e=next((i for i in range(b+1,len(lines)) if re.match(r"\s+extra_args:\s*$",lines[i])),None) if b is not None else None
item=re.match(r"(\s*)-\s",lines[e+1]) if e is not None and e+1<len(lines) else None
if not item: sys.exit("no buildoor_params.extra_args list to add {0} to".format(flag))
lines.insert(e+1,item.group(1)+"- "+flag+"\n")
open(dst,"w").write("".join(lines))
PY
ARGS_FILE="$RENDERED_ARGS"
echo "buildoor: --builder-api-subsidy=$BUILDER_API_SUBSIDY_GWEI, above the config's min_bid ${config_min_bid:-(none)} (rendered -> $ARGS_FILE)"

# The settings file goes to participant 1's VC: Lodestar and Prysm read it from a
# mount at launch, Lighthouse from its validators dir once it is up
if [[ "$ASSERT_MODE" != "manual-config" ]]; then
  case "$SETTINGS_CL" in
    lodestar) fc_flag=--proposerSettingsFile ;;
    prysm) fc_flag=--proposer-settings-file ;;
    lighthouse) fc_flag="" ;;
  esac
  if [[ -n "$fc_flag" ]]; then
    RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
    python3 - "$ARGS_FILE" "$RENDERED_ARGS" "$SETTINGS_FILE" "$FC_MOUNT" <<'PY' || die "cannot add the settings file to $ARGS_FILE"
import json,os,re,sys
src,dst,path,mount=sys.argv[1:5]
name=os.path.basename(path)
lines=open(src).read().splitlines(keepends=True)
i=next((i for i,l in enumerate(lines) if re.match(r"\s*-\s+el_type:",l)),None)
if i is None: sys.exit("no participant to mount {0} into".format(name))
indent=re.match(r"(\s*)-",lines[i]).group(1)+"  "
lines.insert(i+1,indent+"vc_extra_mounts: "+json.dumps({mount:name})+"\n")
body="".join("  "+l if l.strip() else l for l in open(path).read().splitlines(keepends=True))
if lines and not lines[-1].endswith("\n"): lines[-1]+="\n"
lines.append("extra_files:\n  "+json.dumps(name)+": |\n"+"".join("  "+l if l.strip() else "\n" for l in body.splitlines(keepends=True)))
open(dst,"w").write("".join(lines))
PY
    ARGS_FILE="$RENDERED_ARGS"
    VC_EXTRA_PARAMS="${VC_EXTRA_PARAMS:+$VC_EXTRA_PARAMS }$fc_flag=$FC_MOUNT/$(basename "$SETTINGS_FILE")"
  fi
fi

# VC_EXTRA_PARAMS, rendered into a copy of the args file
vc_args=()
for p in $VC_EXTRA_PARAMS; do vc_args+=(--vc-param "$p"); done
if (( ${#vc_args[@]} )); then
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  python3 "$REPO_ROOT/scripts/epbs-sim-helpers.py" render-args "$ARGS_FILE" "$RENDERED_ARGS" "${vc_args[@]}" \
    || die "cannot add ${vc_args[*]} to $ARGS_FILE"
  ARGS_FILE="$RENDERED_ARGS"
  echo "args:    ${vc_args[*]} (rendered -> $ARGS_FILE)"
fi

# clean any stale run (docker-path container is a separate object; the enclave rm
# clears a service-path CB with the rest of the enclave)
docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
kurtosis enclave rm -f "$ENCLAVE" >/dev/null 2>&1 || true

# ---- 1. launch the gloas devnet ---------------------------------------------
log "launching gloas devnet ($ARGS_FILE)"
kurtosis run "$EP_PACKAGE" --enclave "$ENCLAVE" --args-file "$ARGS_FILE" --image-download "$IMAGE_DOWNLOAD"

NET="kt-${ENCLAVE}"
# Discover the CL/VC service names (client-agnostic: cl-1-<cl>-<el>, vc-1-<el>-<cl>),
# so the loop works for lodestar, prysm, etc. without hardcoding the client.
BN_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE 'cl-1-[a-z0-9-]+' | head -1)"
VC_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE 'vc-1-[a-z0-9-]+' | head -1 || true)"
# A validator inside the beacon process (grandine: ethereum-package launches no
# separate grandine VC) serves the keymanager API on the CL service itself.
[[ -n "$VC_SVC" ]] || VC_SVC="$BN_SVC"
[[ -n "$BN_SVC" ]] || die "could not discover a cl-1-* service in $ENCLAVE"
BN="$(kurtosis port print "$ENCLAVE" "$BN_SVC" http)"
VC_KM="$(kurtosis port print "$ENCLAVE" "$VC_SVC" http-validator)"
[[ -n "$BN" && -n "$VC_KM" ]] || die "could not discover BN / VC keymanager ports ($BN_SVC / $VC_SVC)"
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

# ---- 3. render CB config from the live chain -----------------------------------
log "rendering CB config from live BN"
if [[ "$ASSERT_MODE" == "dial" ]]; then
  bo_ip="$(docker network inspect "$NET" -f '{{range .Containers}}{{.Name}} {{.IPv4Address}}{{"\n"}}{{end}}' \
    | awk '/^buildoor--/{split($2,a,"/"); print a[1]}')"
  [[ -n "$bo_ip" ]] || die "dial: cannot find buildoor's address on $NET"
  BUILDOOR_RELAY_URL="$(sed -E "s|@buildoor:|@$bo_ip:|" <<<"$BUILDOOR_RELAY_URL")"
  echo "dial: relay entry $BUILDOOR_RELAY_URL"
fi
GEN_TIME=$(curl -sf "$BN/eth/v1/beacon/genesis" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['genesis_time'])")
echo "genesis_time=$GEN_TIME"
render_cb_config "$RUN_DIR" "$GEN_TIME"

# ---- 4. place commit-boost PBS in the loop (VC -> CB -> buildoor) ------------
if [[ "$CB_LAUNCH" == "service" ]]; then
  log "adding commit-boost PBS as a first-class enclave service ($CB_NAME)"
  # Upload the rendered CB config as a kurtosis files-artifact and add CB as a
  # real enclave service: it gets enclave DNS ($CB_NAME resolvable by the VC and
  # buildoor by name), is torn down by `kurtosis enclave rm`, and shows up in
  # `kurtosis enclave inspect` - no manual container to track.
  kurtosis files upload "$ENCLAVE" "$RUN_DIR" --name "$CB_ARTIFACT" >/dev/null \
    || die "kurtosis files upload of $RUN_DIR failed"
  # CB's log level is [logs.stdout] level in the rendered config; CB does not
  # read RUST_LOG.
  kurtosis service add "$ENCLAVE" "$CB_NAME" "$CB_IMAGE" \
    --files "/cb:$CB_ARTIFACT" \
    --env "CB_CONFIG=/cb/cb-config.toml" \
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
    "$CB_IMAGE" pbs >/dev/null
  sleep 4
  docker ps --format '{{.Names}}' | grep -qx "$CB_NAME" || { docker logs "$CB_NAME" 2>&1 | tail -20; die "CB container exited on boot"; }
  echo "CB up: $(docker ps --filter name=^${CB_NAME}$ --format '{{.Status}}')"
fi

# ---- 5. the VC's builder config (VC -> CB) ------------------------------------
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  write_documented_builder_configs
else
  [[ "$SETTINGS_CL" == lighthouse ]] && install_lighthouse_file_config "$SETTINGS_FILE"
  echo "settings file: no keymanager write; the VC loads $(basename "$SETTINGS_FILE")"
fi
# checked before the activation wait: a missing cap fails here, minutes earlier
if [[ "$ASSERT_MODE" == "trusted-payment" ]]; then
  assert_cap_loaded
fi

# ---- 6a. wait for buildoor to become active on chain -------------------------
# buildoor submits a builder DEPOSIT to the EIP-8282 registry on boot and only
# bids once that deposit is included AND activated (an activation-queue delay,
# empirically ~epoch 4 / slot ~33 on minimal). Until then CB gets 204 "no header
# available". Poll CB's log for buildoor's first bid before observing.
buildoor_live=0
[[ "$WAIT_BUILDOOR" == 1 ]] && log "waiting for buildoor activation (builder deposit -> registry -> active; ~epoch 4)"
for i in $(seq 1 $(( BUILDOOR_ACTIVATION_TIMEOUT / 6 )) ); do
  [[ "$WAIT_BUILDOOR" == 1 ]] || { buildoor_live=1; break; }
  # buildoor's bid reached CB: the debug per-relay receipt (CB with the bid
  # stream), or the bid CB returns to the beacon node, which is the only receipt
  # line before it. grep -c reads the whole log: grep -q exits on the first
  # match, the log reader upstream dies of SIGPIPE, and pipefail fails the test.
  if (( $(cb_logs | grep -cE 'received new header.*relay_id="buildoor|received header .*trustless_bid_eth' || true) > 0 )); then
    buildoor_live=1; break
  fi
  cur=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
  printf '  head=%s waiting for first buildoor bid via CB...\r' "$cur"
  sleep 6
done
echo
(( buildoor_live == 1 )) || { cb_logs | tail -15; die "buildoor never bid through CB within ${BUILDOOR_ACTIVATION_TIMEOUT}s"; }
[[ "$WAIT_BUILDOOR" == 1 ]] && echo "buildoor is active - first bid seen"

# ---- 6b. observe the loop ----------------------------------------------------
# the chain's slot time (nimbus on the mainnet preset runs 12 s slots)
SLOT_SEC=$(curl -sf "$BN/eth/v1/config/spec" | python3 -c \
  "import sys,json;d=json.load(sys.stdin)['data'];print(int(d['SLOT_DURATION_MS'])//1000 if 'SLOT_DURATION_MS' in d else int(d['SECONDS_PER_SLOT']))" \
  2>/dev/null || echo 6)
if (( OBSERVE_FROM_SLOT > 0 )); then
  log "waiting for head slot $OBSERVE_FROM_SLOT before observing"
  while (( $(bn_head_slot) < OBSERVE_FROM_SLOT )); do printf '  head=%s\r' "$(bn_head_slot)"; sleep "$SLOT_SEC"; done
  echo
fi
start_slot=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])")
end_slot=$(( start_slot + OBSERVE_SLOTS ))
log "observing slots ${start_slot}..${end_slot} ($(( OBSERVE_SLOTS * SLOT_SEC ))s)"
for i in $(seq 1 $(( OBSERVE_SLOTS + 3 ))); do
  cur=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
  printf '  head=%s / target=%s\r' "$cur" "$end_slot"
  (( cur >= end_slot )) && break
  sleep "$SLOT_SEC"
done
echo

# ---- 7. assert --------------------------------------------------------------
log "verifying the loop"
# CB-side: bid requests + the bids buildoor returned through CB. CB ranks
# nothing: each request goes to the one relay its auth data names.
CB_LOG=$(cb_logs)
bid_calls=$(grep -c 'execution_payload_bid' <<<"$CB_LOG" || true)
# A CB from before #505's review logs "received new header" with the relay id;
# later ones log "received header" for the bid they return (buildoor is the only relay)
buildoor_bids=$(grep -cE 'received new header.*relay_id="buildoor-mux"|received header .*trustless_bid_eth' <<<"$CB_LOG" || true)

# chain-side: builder-built blocks (the bid names a registry builder)
classified="$(classify_blocks "$start_slot" "$end_slot")"
built_slots="$(sed -n 's/^builder=//p' <<<"$classified")"
payment_only_slots="$(sed -n 's/^payment_only=//p' <<<"$classified")"
n_built=$(count_csv "$built_slots")
n_payment_only=$(count_csv "$payment_only_slots")
# buildoor also bids over p2p, so a builder-built block counts only when its bid
# is one CB returned: same block hash, value and execution payment
cb_log_file="$(mktemp "${TMPDIR:-/tmp}/epbs-cb-log.XXXXXX")"
printf '%s\n' "$CB_LOG" > "$cb_log_file"
sourced="$(python3 "$REPO_ROOT/scripts/epbs-sim-helpers.py" via-cb "$BN" "$start_slot" "$end_slot" "$cb_log_file")" \
  || die "could not match the blocks' bids against CB's"
rm -f "$cb_log_file"
via_cb_slots="$(sed -n 's/^via_cb=//p' <<<"$sourced")"
other_slots="$(sed -n 's/^builder_other=//p' <<<"$sourced")"
n_via_cb=$(count_csv "$via_cb_slots")

echo "  BN -> CB bid calls (window+):     $bid_calls"
echo "  buildoor bids via CB:             $buildoor_bids"
echo "  builder-built blocks on chain:    $n_built  [slots: $built_slots]"
echo "    paid only via execution_payment (value = 0): $n_payment_only"
echo "    built from a bid CB returned:   $n_via_cb  [slots: $via_cb_slots]"
echo "    built from another bid (p2p):   $(count_csv "$other_slots")  [slots: $other_slots]"

# builder-down diverges here: it only needs the builder PROVEN live (>=1
# builder-built slot), then stops buildoor and asserts proposer liveness.
if [[ "$ASSERT_MODE" == "builder-down" ]]; then
  (( n_built >= 1 && buildoor_bids >= 1 )) \
    || { cb_logs | tail -20; die "builder-down precondition: buildoor never built a block (built=$n_built, bids=$buildoor_bids)"; }
  echo "  precondition OK: buildoor built >=1 block before stop"
  assert_builder_down "$start_slot"
  exit 0
fi

# ---- 7b. p2p-floor assertion (opt-in) ---------------------------------------
# The settings file's min_bid (150000000 Gwei) is above buildoor's 101000000 Gwei
# p2p bid and below its 200000000 Gwei builder-API bid, so CB bids survive.
# buildoor's p2p-bidder publishes a competing bid every slot
# (POSTed to the BN's publishExecutionPayloadBid endpoint, which pools it AND
# gossips it: "Published execution payload bid ... value=101000000"), so the BN's
# produceBlockV4 floors it on every builder-built slot:
#   "Ignoring p2p bid below min bid slot=.. bidValue=101000000 minBid=150000000".
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

# manual-config: the page's min_bid does not floor buildoor's p2p bid, so a
# builder-built block may have come over p2p rather than through CB
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  BN_LOG="$(bn_logs)"
  echo "  BN selections via CB / via p2p:   $(grep 'Selected builder block' <<<"$BN_LOG" | grep -c 'bidSource=[^, ]*cb-epbs' || true) / $(grep 'Selected builder block' <<<"$BN_LOG" | grep -c 'bidSource=[^, ]*p2p' || true)"
fi

log "RESULT"
if (( n_via_cb >= MIN_BUILDER_SLOTS && buildoor_bids >= 1 && bid_calls >= 1 && p2p_ok == 1 )); then
  printf '\033[1;32mPASS: %s/%s observed slots built from a bid CB returned (buildoor)\033[0m\n' \
    "$n_via_cb" "$OBSERVE_SLOTS"
  if [[ "$ASSERT_MODE" == "p2p" ]]; then
    printf '\033[1;32mPASS: p2p floor PROVEN (%s rejected); %s blocks selected CB, 0 selected p2p\033[0m\n' \
      "$p2p_rejected" "$cb_selected"
  fi
  # block-submission: also assert the reveal went through the blind pipe
  [[ "$ASSERT_MODE" == "block-submission" ]] && { assert_block_submission; }
  [[ "$ASSERT_MODE" == "trusted-payment" ]]  && { assert_trusted_payment "$start_slot" "$end_slot"; }
  [[ "$ASSERT_MODE" == "dial" ]]             && { assert_dial; }
  exit 0
else
  cb_logs | tail -30
  (( p2p_ok == 1 )) || die "p2p assertion failed (rejected=$p2p_rejected need>=1, cb_selected=$cb_selected need>=1, p2p_selected=$p2p_selected need=0)"
  die "loop not satisfied (built from a CB bid=$n_via_cb need>=$MIN_BUILDER_SLOTS, builder-built=$n_built, buildoor bids=$buildoor_bids, bid calls=$bid_calls)"
fi
