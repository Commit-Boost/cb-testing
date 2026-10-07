#!/usr/bin/env bash
# run-epbs-sim.sh - reproducible ePBS (gloas) + commit-boost + keymanager loop.
#
# Stands up a gloas devnet with buildoor (gloas_fork_epoch 0, minimal, 6s slots),
# lodestar CL+VC with keymanager enabled, adds a commit-boost PBS sidecar as a
# first-class kurtosis enclave service (CB_LAUNCH=service; docker = legacy path),
# runs `cb-km apply` to point every validator's builder_config at commit-boost
# (auth_data = buildoor's hostname), then observes the loop and asserts:
#   BN -> CB execution_payload_bid calls, buildoor bids via CB ("received new
#   header" from buildoor-mux; CB forwards each request to the one relay its
#   auth data names and ranks nothing), and builder-built blocks on chain (the
#   block's signed_execution_payload_bid
#   names a registry builder, not BUILDER_INDEX_SELF_BUILD; see
#   scripts/epbs-blocks.py).
#
# A real ethereum-package `epbs` mev_type that launches CB as the sidecar (no
# manual add + no `cb-km apply`) needs an upstream-package change - see the
# "Native ePBS mev_type: investigation & submodule-upgrade blockers" in docs/EPBS.md.
#
# Opt-in assertion modes turn a merged feature into a live regression check:
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
#   --assert block-submission  also assert the blind-pipe reveal at POST
#                      /eth/v1/builder/beacon_blocks: the proposer reveals each
#                      builder-built block through CB, which forwards the SSZ
#                      bytes to buildoor unparsed. HARD-fails unless >=1 reveal
#                      reached the forward (2xx = buildoor accepted, 5xx = it
#                      answered without accepting) with zero 4xx (missing gloas
#                      Eth-Consensus-Version, or a non-SSZ body) and zero
#                      forwards buildoor never answered. CB decodes nothing, so
#                      any preset works.
#   --assert builder-down  after buildoor activates and >=1 builder-built slot is
#                      seen, STOP buildoor and assert the proposer stays live: the
#                      chain keeps advancing, the new blocks are self-built
#                      (builder_index == BUILDER_INDEX_SELF_BUILD), and CB returns
#                      204 "no header available" and NEVER 500. Proves builder
#                      failure never stalls the proposer.
#   --assert trusted-payment  set [pbs] max_execution_payment_gwei = "unclamped"
#                      (the relays carry none, so it is the effective cap), start
#                      the lodestar VC with --allowDangerousTrustedPayments, and
#                      tell buildoor to claim half of each bid as execution_payment
#                      (--builder-api-execution-payment-percent). HARD-fails unless
#                      cb-km stored 18446744073709551615 on every builder_config
#                      entry of every key, the BN selects CB's execution-payment
#                      bid in >= MIN_BUILDER_SLOTS slots, and for each of them the
#                      BN values the bid CB forwarded at value + execution_payment.
#                      First runs a CONTROL devnet WITHOUT the flag (skip with
#                      TRUSTED_PAYMENT_CONTROL=0): cb-km apply must exit non-zero
#                      with a 400 on every key, the VC naming the flag.
#   --assert manual-config  run a CB commit that has no cb-km (CB_COMMIT) and write
#                      each validator key's builder_config with the curl call in
#                      that commit's docs/docs/get_started/epbs.md, run as written
#                      with the sim's CB URL and buildoor's hostname auth data in
#                      place of the page's example values. The CB config carries
#                      only keys the commit's config code names. Then the default
#                      builder-built assertion.
#   --assert config-matrix  measure how the VC stores and applies each builder
#                      config value, per client. The 64 keys split into seven
#                      [[mux]] groups with different values (scripts/epbs-config-
#                      matrix.py lists them and what each discriminates); cb-km
#                      writes them, buildoor bids value 0.2 ETH + execution_payment
#                      0.2 ETH whatever the proposer advertised, and every slot of
#                      OBSERVE_SLOTS (64) is classified by its proposer's group as
#                      via CB, another builder's bid, self-built or missed. Records
#                      the stored doc of every key next to the doc cb-km wrote, the
#                      client's own per-slot bid-selection lines and CB's bid
#                      requests and builder preferences per group, all under CM_OUT.
#                      Then checks cb-km: --dry-run writes nothing, a plain apply
#                      drops a third-party entry, --preserve-entries keeps it, and
#                      --prune is refused when a mux has a URL loader. Reports the
#                      client's behaviour without judging it; fails only when the
#                      observation is incomplete (a group with fewer than
#                      CM_MIN_PROPOSALS blocks, an unclassified slot) or a cb-km
#                      check fails. cb-km apply may exit non-zero here (a VC that
#                      refuses a group's values); its output is recorded.
# No flag = the default builder-built assertion (unchanged).
#
# --render-only DIR renders the mode's CB config + km overlay into DIR through the
# same code path as a live run (placeholder genesis time + keymanager URL), runs
# `cb-km apply --dry-run` on them and exits: no docker, no kurtosis (cb-km is
# CB_KM_BIN, else the submodule's, which cargo rebuilds if it is stale).
#
# One devnet at a time (~15G). Usage:
#   scripts/run-epbs-sim.sh [--assert p2p|preserve|block-submission|builder-down|trusted-payment|manual-config|config-matrix] [--render-only DIR]
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
# Build the CB image (and cb-km) from this commit instead of the submodule's
# checkout; it must be in the submodule's repo (scripts/ensure-cb-artifacts.sh).
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
CB_KM_BIN="${CB_KM_BIN:-}"
# The relay CB bids into, rendered into BOTH __BUILDOOR_RELAY_URL__ sites in
# cb-config.toml.tmpl: the top-level [[relays]] entry and the [[mux.relays]]
# entry the listed validator pubkeys route to. Substituting only one leaves the
# validators bidding to the other relay while the file reads as if retargeted.
# The URL embeds the relay's BLS pubkey, so pointing at a different relay means
# supplying its pubkey too. Default = the devnet's buildoor.
BUILDOOR_RELAY_URL="${BUILDOOR_RELAY_URL:-http://0x8de7ec501d574152f52a962bf588573df2fc3563fd0c6077651208ed20f24f3d8572425706b343117b48bdca56808416@buildoor:8080}"
BUILDOOR_ACTIVATION_TIMEOUT="${BUILDOOR_ACTIVATION_TIMEOUT:-1200}"  # s to wait for the builder deposit to activate (queue delay varies run to run, seen out to ~slot 100)
OBSERVE_SLOTS="${OBSERVE_SLOTS:-}"       # slots to watch once buildoor is active (16; config-matrix 64)
MIN_BUILDER_SLOTS="${MIN_BUILDER_SLOTS:-8}"  # PASS threshold (builder-built via CB; allows some missed slots)
KEEP="${KEEP:-0}"                        # 1 = leave the enclave + CB running
RUN_DIR="configs/epbs/.run"              # rendered (gitignored) config lives here
# Extra params for participant 1's VC, space-separated, rendered into a copy of the
# args file (e.g. VC_EXTRA_PARAMS=--builder.selection=maxprofit on lodestar).
VC_EXTRA_PARAMS="${VC_EXTRA_PARAMS:-}"
# 0 = do not wait for buildoor's first bid via CB before observing: for a client
# whose beacon node never requests bids (config-matrix on nimbus)
WAIT_BUILDOOR="${WAIT_BUILDOOR:-1}"
# observe from this head slot at the earliest (e.g. past a gloas fork at epoch 1)
OBSERVE_FROM_SLOT="${OBSERVE_FROM_SLOT:-0}"
# config-matrix: where the evidence lands, the fewest blocks each group must
# propose for the observation to count, and buildoor's bid: a 0.4 ETH bid, half
# claimed as execution_payment whatever max_execution_payment the proposer
# advertised, so the beacon node's own cap alone decides what counts
CM_OUT="${CM_OUT:-$RUN_DIR/config-matrix}"
CM_MIN_PROPOSALS="${CM_MIN_PROPOSALS:-3}"
CM_BUILDOOR_ARGS="${CM_BUILDOOR_ARGS:---builder-api-subsidy=400000000 --builder-api-execution-payment-percent=50 --builder-api-ignore-preference-limit}"

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
  ""|p2p|preserve|block-submission|builder-down|trusted-payment|manual-config|config-matrix|dial) : ;;
  ladder) no_mode "retired, CB plays no timing games on the ePBS bid path" ;;
  request-auth) no_mode "retired, CB no longer verifies the request auth (verify_builder_request_auth is gone); the builder does" ;;
  direct) no_mode "retired, cb-km writes no direct builder entries (the overlay refuses direct_entries); with CB down the beacon node falls back to p2p bids and local blocks" ;;
  pipe) no_mode "renamed: --assert dial" ;;
  *) printf 'unknown --assert mode: %s (want p2p|preserve|block-submission|builder-down|trusted-payment|manual-config|config-matrix|dial)\n' "$ASSERT_MODE" >&2; exit 2 ;;
esac
# config-matrix needs enough slots for every group to propose several times
[[ -n "$OBSERVE_SLOTS" ]] || { [[ "$ASSERT_MODE" == "config-matrix" ]] && OBSERVE_SLOTS=64 || OBSERVE_SLOTS=16; }

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
# trusted-payment: cb-km writes [pbs] max_execution_payment_gwei = "unclamped" as
# the largest Uint64, so the BN counts the whole execution payment
UNCLAMPED_CAP=18446744073709551615
# trusted-payment: 1 = first run the control devnet, whose VC lacks the flag
TRUSTED_PAYMENT_CONTROL="${TRUSTED_PAYMENT_CONTROL:-1}"
# internal: "control" marks the control phase's own run of this script
TRUSTED_PAYMENT_PHASE="${TRUSTED_PAYMENT_PHASE:-}"

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
#   202 -> forwarded. CB's own log says whether a builder accepted it
#          ("signed beacon block accepted") or none did ("no builder accepted"),
#          which is expected while buildoor gets the block over p2p.
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
  accepted=$(grep -c 'signed beacon block accepted' <<<"$CB_LOG" || true)
  none_accepted=$(grep -c 'no builder accepted the signed beacon block' <<<"$CB_LOG" || true)
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

# CB's URL as the VC calls it: the km overlay's advertised_url, and the url of
# the builder_config entry cb-km writes on every key
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

# --- assert trusted-payment, control: a VC without the flag refuses the cap ------
# Run in the control phase's own devnet, whose lodestar VC lacks
# --allowDangerousTrustedPayments. lodestar's keymanager refuses any
# builder_config entry with max_execution_payment > 0 (400, naming the flag), so
# cb-km apply of the capped config must exit non-zero with a 400 on every key,
# and no key may end up holding the cap. POSTing one projected doc by hand (the
# first one `cb-km apply --dry-run` prints) ties the 400 to the flag, not to some
# other rejection.
assert_trusted_payment_refused() {
  log "trusted-payment control: cb-km apply of a capped config, VC WITHOUT --allowDangerousTrustedPayments"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  local out rc=0
  out="$("$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" 2>&1 \
    | sed -r 's/\x1b\[[0-9;]*m//g')" || rc=$?
  local n_keys refused
  n_keys="$(curl -sf -H "Authorization: Bearer $KM_TOKEN" "$VC_KM/eth/v1/keystores" \
    | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["data"]))')" \
    || die "control: could not list the VC's keystores"
  # cb-km reports each refusal on stderr as "ERROR: <vc>: POST <key> failed:
  # 400 Bad Request: <VC message>"; an older cb-km repeats "... failed:" before
  # the status. || true: no match is a 0 here, not a reason for set -e to end the run
  refused="$(grep -E 'POST 0x[0-9a-f]{96} failed: (.*failed: )?400' <<<"$out" \
    | grep -oE 'POST 0x[0-9a-f]{96}' | sort -u | wc -l || true)"
  echo "  cb-km apply exit code:            $rc (need non-zero)"
  echo "  keys refused with HTTP 400:       $refused of $n_keys (need all)"
  { grep -m1 -E 'failed: 400' <<<"$out" || true; } | cut -c1-240 | sed 's/^/    e.g. /'

  # one projected doc, POSTed by hand to read the VC's reason: the first
  # "would apply <key>: <doc>" line of a dry run
  local dry line key doc code body stored_cap
  dry="$("$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --dry-run 2>&1)" \
    || die "control: cb-km apply --dry-run failed: $(tail -3 <<<"$dry")"
  line="$(grep -m1 '^would apply ' <<<"$dry" || true)"
  [[ "$line" =~ ^would\ apply\ (0x[0-9a-f]{96}):\ (\{.*\})$ ]] \
    || die "control: cb-km apply --dry-run printed no 'would apply <key>: <doc>' line"
  key="${BASH_REMATCH[1]}"; doc="${BASH_REMATCH[2]}"
  body="$(curl -s -w '\n%{http_code}' -X POST -H "Authorization: Bearer $KM_TOKEN" -H 'Content-Type: application/json' \
    --data-binary "$doc" "$VC_KM/eth/v1/validator/$key/builder_config")"
  code="$(tail -1 <<<"$body")"; body="$(sed '$d' <<<"$body")"
  stored_cap="$(km_get "$key" | python3 -c \
    'import sys,json;d=json.load(sys.stdin)["data"];print(max([int(e.get("max_execution_payment") or 0) for e in d.get("builders") or []],default=0))')" \
    || die "control: could not read $key's stored builder_config"
  echo "  POST by hand of $key's projected doc: HTTP $code $body"
  echo "  largest cap stored on that key:   $stored_cap (need 0)"

  log "RESULT"
  if (( rc != 0 && n_keys >= 1 && refused == n_keys )) && [[ "$code" == 400 && "$body" == *allowDangerousTrustedPayments* && "$stored_cap" == 0 ]]; then
    printf '\033[1;32mPASS: without --allowDangerousTrustedPayments, cb-km apply exited %s with a 400 on all %s keys and stored no cap\033[0m\n' "$rc" "$n_keys"
    return 0
  fi
  die "control: want cb-km exit != 0 (got $rc), 400 on all $n_keys keys (got $refused), a 400 naming the flag on the hand POST (got $code), stored cap 0 (got $stored_cap)"
}

# --- assert trusted-payment: cb-km stored the unclamped cap on every entry ---------
# The relays carry no max_execution_payment_gwei, so [pbs] "unclamped" holds the
# effective cap and cb-km must write it, as the largest Uint64, onto every
# builder_config entry of every key (a relay-only projection leaves the entries
# uncapped, so the BN values the trusted payment at 0).
assert_cap_projected() {
  log "assert trusted-payment: cb-km stored max_execution_payment = $UNCLAMPED_CAP on every entry"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  python3 - "$VC_KM" "$KM_TOKEN" "$UNCLAMPED_CAP" <<'PY' || die "trusted-payment: the cap is missing from stored builder_config entries"
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
# A CB commit without cb-km documents the keymanager call an operator makes
# instead. This mode makes that call, taken from the page at the commit under
# test, so a documented call that does not route the proposer's bid requests to
# CB fails the run.
DOCS_PAGE="docs/docs/get_started/epbs.md"
# the page's example values, swapped for the sim's before the call runs
DOCS_CB_URL="http://cb.example.com:18550"
DOCS_AUTH_DATA="0x6275696c6465722d612e6578616d706c652e636f6d"   # builder-a.example.com

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
  [[ "$call" == curl* && "$call" == *"/builder_config"* ]] \
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
# crates/common/src, where Commit-Boost parses its config. A commit without
# cb-km's guard ignores an unknown [pbs] or [[mux]] key, so a key it does not
# know would load as if it took effect.
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
write_dial_builder_configs() {
  log "dial: each key's builder config: url=$CB_ENTRY_URL auth_data=$DIAL_AUTH_URL"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  local keys body code n=0
  keys="$(curl -sf -H "Authorization: Bearer $KM_TOKEN" "$VC_KM/eth/v1/keystores" \
    | python3 -c 'import sys,json;[print(k["validating_pubkey"]) for k in json.load(sys.stdin)["data"]]')" \
    || die "dial: could not list the VC's keystores"
  body="$(printf '{"builders":[{"url":"%s","auth_data":"0x%s","builder_pubkeys":[]}]}' "$CB_ENTRY_URL" \
    "$(python3 -c 'import sys;print(sys.argv[1].encode().hex())' "$DIAL_AUTH_URL")")"
  for key in $keys; do
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $KM_TOKEN" \
      -H 'Content-Type: application/json' -d "$body" "$VC_KM/eth/v1/validator/$key/builder_config")"
    [[ "$code" == 2* ]] || die "dial: builder_config POST for $key got HTTP $code"
    n=$((n + 1))
  done
  echo "  wrote $n builder configs"
}

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
    code="$(KEYMANAGER_URL="$VC_KM" TOKEN="$KM_TOKEN" PUBKEY="$key" RESP="$resp" bash -c \
      'curl() { command curl -sS -o "$RESP" -w "%{http_code}" "$@"; }; . "$1"' _ "$call")" || true
    [[ "$code" == 2* ]] || die "manual-config: the documented call for $key got HTTP $code: $(head -c 300 "$resp" 2>/dev/null)"
    km_entry_urls "$key" | grep -qxF "$CB_ENTRY_URL" \
      || die "manual-config: $key stores no $CB_ENTRY_URL entry after HTTP $code"
    n=$(( n + 1 ))
  done
  echo "  $n keys written (HTTP $code), e.g. $key: $(km_get "$key")"
}

# --- config-matrix: per-group builder config, what the VC stores and applies ---
CM_PY="$REPO_ROOT/scripts/epbs-config-matrix.py"
cm_group_key() { # $1=group index $2=key index -> that key of cm-groups.json
  python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))[int(sys.argv[2])]["keys"][int(sys.argv[3])])' \
    "$RUN_DIR/cm-groups.json" "$1" "$2"
}

# cb-km apply of every group. A VC may refuse a group's values (lodestar refuses
# a nonzero max_execution_payment without --allowDangerousTrustedPayments), which
# makes apply exit non-zero: that is a measurement here, so the exit code and the
# refusals are recorded, and only an apply that wrote no key at all stops the run.
cm_apply() {
  log "config-matrix: cb-km apply, one mux per group"
  mkdir -p "$CM_OUT"
  cp "$RUN_DIR/cb-config.toml" "$RUN_DIR/km-overlay.toml" "$RUN_DIR/cm-groups.json" "$RUN_DIR/km-dry-run.txt" "$CM_OUT/"
  local rc=0 accepted errors
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" \
    > "$CM_OUT/km-apply.txt" 2>&1 || rc=$?
  echo "$rc" > "$CM_OUT/km-apply.rc"
  accepted=$(grep -c '^accepted: ' "$CM_OUT/km-apply.txt" || true)
  errors=$(grep -c '^ERROR: ' "$CM_OUT/km-apply.txt" || true)
  echo "  cb-km apply exit $rc: $accepted keys accepted, $errors errors"
  { grep -m2 '^ERROR: ' "$CM_OUT/km-apply.txt" || true; } | cut -c1-300 | sed 's/^/    /'
  (( accepted >= 1 )) || die "config-matrix: cb-km apply wrote no key (see $CM_OUT/km-apply.txt)"
  log "config-matrix: what each VC stores (GET) against what cb-km wrote"
  python3 "$CM_PY" stored "$VC_KM" "$RUN_DIR/km-token.txt" "$RUN_DIR/cm-groups.json" \
    "$RUN_DIR/km-dry-run.txt" "$CM_OUT/stored-docs.json" | tee "$CM_OUT/stored.txt" \
    || echo "WARN: reading the stored docs failed (see above); the observation goes on"
}

# cb-km's own flags against the live VC, once the observation is over:
#   --dry-run          a projection that would change a key prints it and writes nothing
#   plain apply        drops a third-party entry on a key (K2)
#   --preserve-entries keeps the third-party entry beside CB's (K1)
#   --prune            refused when a mux uses a URL loader, before any VC is contacted
cm_km_checks() {
  log "config-matrix: cb-km --dry-run, plain apply, --preserve-entries and --prune"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  local d="$CM_OUT/km-checks" K1 K2 before after rc code ok fails=0
  mkdir -p "$d"; : > "$d/summary.txt"
  check() { # $1=name $2=0 pass / non-zero fail $3=detail
    local v=PASS; [[ "$2" == 0 ]] || { v=FAIL; fails=$(( fails + 1 )); }
    printf '  %-34s %s  %s\n' "$1" "$v" "$3" | tee -a "$d/summary.txt"
  }
  K1="$(cm_group_key 0 0)"; K2="$(cm_group_key 0 1)"   # two g-default keys

  # --dry-run: a config giving g-default boost 55 is printed, not written
  sed '/^id = "g-default"$/a builder_boost_factor = 55' "$RUN_DIR/cb-config.toml" > "$d/dry-config.toml"
  # each check runs as `{ ...; } || ok=1`, so a failing condition is a FAIL row, not a set -e exit
  before="$(km_get "$K1" || true)"
  rc=0; "$CB_KM_BIN" apply --config "$d/dry-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --dry-run > "$d/dry-run.txt" 2>&1 || rc=$?
  after="$(km_get "$K1" || true)"
  printf '%s\n%s\n' "$before" "$after" > "$d/dry-run-get-before-after.json"
  ok=0; { grep -q "^would apply $K1: .*\"builder_boost_factor\":\"55\"" "$d/dry-run.txt" && grep -q 'nothing sent' "$d/dry-run.txt" \
    && [[ "$rc" == 0 && -n "$before" && "$before" == "$after" ]]; } || ok=1
  check "--dry-run writes nothing" $ok "exit $rc; would apply boost 55 on $K1; stored doc unchanged: $([[ "$before" == "$after" ]] && echo yes || echo NO)"

  # plain apply drops a third-party entry, --preserve-entries keeps it
  local TP_URL="http://third-party-relay:19999" TP_AUTH="0x74686972647061727479"   # "thirdparty"
  local TP_BP="0x8de7ec501d574152f52a962bf588573df2fc3563fd0c6077651208ed20f24f3d8572425706b343117b48bdca56808416"
  local TP_DOC="{\"builders\":[{\"url\":\"$TP_URL\",\"auth_data\":\"$TP_AUTH\",\"builder_pubkeys\":[\"$TP_BP\"]}]}"
  code="$(km_post "$K2" "$TP_DOC")"
  rc=0; "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" > "$d/apply-plain.txt" 2>&1 || rc=$?
  km_get "$K2" > "$d/K2-after-plain-apply.json" || true
  ok=0; { km_entry_urls "$K2" | grep -qxF "$CB_ENTRY_URL" && ! km_entry_urls "$K2" | grep -qF "$TP_URL" && [[ "$code" == 2* ]]; } || ok=1
  check "plain apply drops a 3rd-party entry" $ok "POST 3rd-party to $K2: HTTP $code; apply exit $rc; entries now: $(km_entry_urls "$K2" | paste -sd,)"
  code="$(km_post "$K1" "$TP_DOC")"
  rc=0; "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --preserve-entries > "$d/apply-preserve.txt" 2>&1 || rc=$?
  km_get "$K1" > "$d/K1-after-preserve.json" || true
  ok=0; { km_entry_urls "$K1" | grep -qxF "$CB_ENTRY_URL" && km_entry_urls "$K1" | grep -qF "$TP_URL" && [[ "$code" == 2* ]]; } || ok=1
  check "--preserve-entries keeps it" $ok "POST 3rd-party to $K1: HTTP $code; apply exit $rc; entries now: $(km_entry_urls "$K1" | paste -sd,)"

  # --prune with a URL-loader mux: refused before any VC is contacted
  { cat "$RUN_DIR/cb-config.toml"
    printf '\n[[mux]]\nid = "url-loader"\nloader = { url = "http://127.0.0.1:9/keys.json" }\n\n'
    printf '[[mux.relays]]\nid = "buildoor-url-loader"\nurl = "%s"\n' "$BUILDOOR_RELAY_URL"
  } > "$d/url-loader-config.toml"
  before="$(km_get "$K2" || true)"
  rc=0; "$CB_KM_BIN" apply --config "$d/url-loader-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --prune > "$d/prune.txt" 2>&1 || rc=$?
  after="$(km_get "$K2" || true)"
  ok=0; { [[ "$rc" != 0 && "$before" == "$after" ]] && grep -q -- '--prune would clear' "$d/prune.txt" && ! grep -q '^accepted: ' "$d/prune.txt"; } || ok=1
  check "--prune refused with a URL loader" $ok "exit $rc: $(sed -r 's/\x1b\[[0-9;]*m//g' "$d/prune.txt" | grep -m1 -- '--prune' | cut -c1-120)"
  # the same config without --prune is accepted, so the refusal is about --prune
  rc=0; "$CB_KM_BIN" apply --config "$d/url-loader-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --dry-run > "$d/url-loader-dry-run.txt" 2>&1 || rc=$?
  ok=0; { [[ "$rc" == 0 ]] && grep -q 'projects none of its keys' "$d/url-loader-dry-run.txt"; } || ok=1
  check "  same config, no --prune: loads" $ok "dry-run exit $rc: $(grep -m1 'WARN' "$d/url-loader-dry-run.txt" | cut -c1-110)"
  return $(( fails > 0 ))
}

# After the observation window: save the clients' command lines and full logs,
# classify every slot by its proposer's group, then run the cb-km checks.
cm_report() { # $1,$2 = observed window
  log "config-matrix: classify slots $1..$2 by the proposer's group"
  mkdir -p "$CM_OUT/logs"
  kurtosis enclave inspect "$ENCLAVE" > "$CM_OUT/inspect.txt" 2>&1 || true
  local svc vc_log="$CM_OUT/logs/$VC_SVC.log" rc=0 km_rc=0
  for svc in $(grep -oE '\b(cl-1-[a-z0-9-]+|vc-1-[a-z0-9-]+|buildoor[a-z0-9-]*)\b' "$CM_OUT/inspect.txt" | sort -u); do
    kurtosis service inspect "$ENCLAVE" "$svc" > "$CM_OUT/inspect-$svc.txt" 2>&1 || true
    kurtosis service logs -a "$ENCLAVE" "$svc" 2>&1 | sed -r 's/\x1b\[[0-9;]*m//g' > "$CM_OUT/logs/$svc.log" || true
  done
  cb_logs > "$CM_OUT/logs/$CB_NAME.log"
  echo "$1 $2" > "$CM_OUT/window.txt"
  CM_MIN_PROPOSALS="$CM_MIN_PROPOSALS" python3 "$CM_PY" classify "$BN" "$1" "$2" "$RUN_DIR/cm-groups.json" \
    "$CM_OUT/logs/$CB_NAME.log" "$CM_OUT/logs/$BN_SVC.log" "$vc_log" "$CM_OUT" || rc=$?
  cm_km_checks || km_rc=$?
  log "RESULT"
  if (( rc == 0 && km_rc == 0 )); then
    printf '\033[1;32mPASS: config-matrix observed every group (>= %s blocks each) and the cb-km checks passed; evidence in %s\033[0m\n' \
      "$CM_MIN_PROPOSALS" "$CM_OUT"
    return 0
  fi
  (( rc == 0 )) || echo "  the observation is incomplete (see $CM_OUT/table.txt)"
  (( km_rc == 0 )) || echo "  a cb-km check failed (see $CM_OUT/km-checks/summary.txt)"
  return 1
}

# ---- CB config + km overlay rendering ---------------------------------------
# One code path for a live run, the pre-launch dry run and --render-only, so
# what cb-km dry-runs is what CB loads.
render_cb_config() { # $1=out dir $2=genesis_time $3=VC keymanager URL
  local out="$1" gen_time="$2" vc_km="$3"
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
  # manual-config runs no cb-km, so it gets no overlay
  [[ "$ASSERT_MODE" == "manual-config" ]] || \
  sed -e "s|__VC_KM_URL__|$vc_km|" \
      -e "s|__TOKEN_PATH__|$(cd "$out" && pwd)/km-token.txt|" \
      configs/epbs/km-overlay.toml.tmpl > "$out/km-overlay.toml"
  grep -q '__' "$out/cb-config.toml" && die "unrendered placeholder in cb-config.toml"

  # manual-config: a commit without cb-km does not read its p2p projection keys
  if [[ "$ASSERT_MODE" == "manual-config" ]]; then
    sed -i -E -e '/^(min_bid_p2p_eth|builder_boost_factor_p2p) = /d' \
      -e '/^# projection-only \(KM tooling\)/d' -e '/^# 0\.101 ETH p2p bid; entries keep/d' "$out/cb-config.toml"
  fi

  # trusted-payment: the [pbs] cap is the only max_execution_payment_gwei in the
  # file, so it is the effective cap for both relay entries; cb-km writes it onto
  # every builder_config entry as the largest Uint64.
  if [[ "$ASSERT_MODE" == "trusted-payment" ]]; then
    sed -i 's|^skip_sigverify = .*$|&\nmax_execution_payment_gwei = "unclamped"|' "$out/cb-config.toml"
    [[ "$(grep -c '^max_execution_payment_gwei = ' "$out/cb-config.toml")" == 1 ]] \
      && grep -q '^max_execution_payment_gwei = "unclamped"$' "$out/cb-config.toml" \
      || die "trusted-payment: want exactly one max_execution_payment_gwei ([pbs]) in cb-config.toml"
  fi

  # config-matrix: one mux per group of keys, each with its own values and relay
  # entry; the group list lands beside the config
  if [[ "$ASSERT_MODE" == "config-matrix" ]]; then
    python3 "$REPO_ROOT/scripts/epbs-config-matrix.py" render-config "$out/cb-config.toml" "$out/cm-groups.json" \
      || die "config-matrix: could not split the template's mux into groups"
  fi
}

# cb-km refuses a [pbs] or [[mux]] key Commit-Boost does not read, an unknown
# relay key (as CB does) and any overlay key but advertised_url and [[vcs]], so a
# dry run proves the rendered pair loads. The projected docs land in
# $1/km-dry-run.txt.
km_dry_run() { # $1=dir holding cb-config.toml + km-overlay.toml
  "$CB_KM_BIN" apply --config "$1/cb-config.toml" --overlay "$1/km-overlay.toml" --dry-run \
    > "$1/km-dry-run.txt" 2>&1 \
    || { tail -20 "$1/km-dry-run.txt"; die "cb-km apply --dry-run refused $1/cb-config.toml or $1/km-overlay.toml"; }
  echo "cb-km dry run: $(sed -r 's/\x1b\[[0-9;]*m//g' "$1/km-dry-run.txt" | grep -oE 'dry-run: .*' | tail -1)"
}

# ---- render-only: one mode's CB config + overlay, checked by cb-km, no devnet --
if [[ -n "$RENDER_ONLY" && "$ASSERT_MODE" == "manual-config" ]]; then
  # no cb-km: check the keys against the commit and print the call the run makes
  CB_SHA="$(cb_commit)"
  render_cb_config "$RENDER_ONLY" 0 "http://127.0.0.1:5062"
  check_config_keys "$RENDER_ONLY/cb-config.toml" "$CB_SHA"
  docs_builder_config_call "$CB_SHA" > "$RENDER_ONLY/builder-config-call.sh"
  echo "rendered manual-config @ ${CB_SHA:0:12}: $RENDER_ONLY/cb-config.toml $RENDER_ONLY/builder-config-call.sh"
  sed 's/^/    /' "$RENDER_ONLY/builder-config-call.sh"
  exit 0
fi
if [[ -n "$RENDER_ONLY" ]]; then
  if [[ -z "$CB_KM_BIN" ]]; then
    # the submodule's cb-km, rebuilt by cargo if it is not from the pinned commit
    cb_art="$(bash "$REPO_ROOT/scripts/ensure-cb-artifacts.sh" --km-only)" \
      || die "provisioning cb-km from the submodule failed (see build output above)"
    eval "$cb_art"   # sets CB_KM_BIN
  fi
  [[ -x "$CB_KM_BIN" ]] || die "cb-km at $CB_KM_BIN is not executable"
  render_cb_config "$RENDER_ONLY" 0 "http://127.0.0.1:5062"
  echo "rendered ${ASSERT_MODE:-default}: $RENDER_ONLY/cb-config.toml $RENDER_ONLY/km-overlay.toml"
  km_dry_run "$RENDER_ONLY"
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
# Provision the ePBS CB artifacts (image + cb-km) from the pinned submodule,
# sha-tagged so a stale build can never be silently used. Skipped when the caller
# pins CB_IMAGE explicitly (advanced path; CB_KM_BIN then falls back to discovery).
if [[ -z "$CB_IMAGE" ]]; then
  # Capture stdout (the two eval-able lines); the script streams build output to
  # stderr live. Hard-fail on a build error rather than silently continuing with
  # an empty CB_IMAGE (the "silence == success" trap).
  km_flag=""; [[ "$ASSERT_MODE" == "manual-config" ]] && km_flag="--no-km"
  cb_art="$(bash "$(dirname "$0")/ensure-cb-artifacts.sh" $km_flag)" \
    || die "provisioning CB artifacts from the submodule failed (see build output above)"
  km_override="$CB_KM_BIN"
  eval "$cb_art"   # sets CB_IMAGE + CB_KM_BIN
  # a caller's CB_KM_BIN (e.g. a cb-km branch build) wins over the submodule's
  [[ -n "$km_override" ]] && CB_KM_BIN="$km_override"
fi
docker image inspect "$CB_IMAGE" >/dev/null 2>&1 || die "CB image $CB_IMAGE not present locally"

if [[ "$ASSERT_MODE" != "manual-config" ]]; then
  if [[ -z "$CB_KM_BIN" ]]; then
    if command -v cb-km >/dev/null 2>&1; then CB_KM_BIN="$(command -v cb-km)"
    else die "cb-km binary not found - set CB_KM_BIN (build: cargo build -p cb-km-tool --release)"; fi
  fi
  "$CB_KM_BIN" --help >/dev/null 2>&1 || die "cb-km at $CB_KM_BIN not runnable"
fi
# preserve mode needs the merged --preserve-entries flag (epbs branch)
if [[ "$ASSERT_MODE" == "preserve" ]]; then
  "$CB_KM_BIN" apply --help 2>&1 | grep -q -- '--preserve-entries' \
    || die "cb-km at $CB_KM_BIN lacks --preserve-entries; build a commit-boost-client commit with it \
(cargo build -p cb-km-tool --release) and set CB_KM_BIN"
fi
# Every builder-built count below rests on this classifier; check it against its
# boundary cases before spending a devnet on it.
python3 scripts/epbs-blocks.py self-test || die "scripts/epbs-blocks.py self-test failed"
# Render this mode's CB config with placeholder chain values and dry-run cb-km
# on it now: a key it refuses would otherwise surface only once the devnet is up.
render_cb_config "$RUN_DIR" 0 "http://127.0.0.1:5062"
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  CB_SHA="$(cb_commit)"
  check_config_keys "$RUN_DIR/cb-config.toml" "$CB_SHA"
  docs_builder_config_call "$CB_SHA" >/dev/null
  echo "docs:    $DOCS_PAGE @ ${CB_SHA:0:12}"
else
  km_dry_run "$RUN_DIR"
fi
avail_g=$(free -g | awk '/^Mem:/{print $7}')
(( avail_g >= 10 )) || echo "WARN: only ${avail_g}G RAM available; a devnet wants ~15G"
echo "cb-km:   ${CB_KM_BIN:-none}"
echo "CB img:  $CB_IMAGE"
echo "CB join: $CB_LAUNCH"
echo "assert:  ${ASSERT_MODE:-default (builder-built)}"
echo "enclave: $ENCLAVE   observe: ${OBSERVE_SLOTS} slots   pass>=${MIN_BUILDER_SLOTS}"

# trusted-payment: the control needs a VC WITHOUT --allowDangerousTrustedPayments,
# and the flag is fixed for the VC's lifetime, so the control is its own devnet:
# a run of this script in the control phase (it stops after cb-km apply, ~1-2
# min, and removes its enclave) before this run launches with the flag.
if [[ "$ASSERT_MODE" == "trusted-payment" && -z "$TRUSTED_PAYMENT_PHASE" && "$TRUSTED_PAYMENT_CONTROL" == "1" ]]; then
  log "trusted-payment control phase (own devnet, VC without --allowDangerousTrustedPayments)"
  TRUSTED_PAYMENT_PHASE=control KEEP=0 CB_IMAGE="$CB_IMAGE" CB_KM_BIN="$CB_KM_BIN" \
    bash "$REPO_ROOT/scripts/run-epbs-sim.sh" --assert trusted-payment \
    || die "trusted-payment control phase failed (its output is above)"
  log "trusted-payment control phase passed; launching the devnet with the flag"
fi

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

# trusted-payment: lodestar's VC refuses a builder_config entry whose
# max_execution_payment is above 0 unless started with
# --allowDangerousTrustedPayments. Render a copy of the args file with the flag
# added to the first vc_extra_params list (participant 1 = the VC cb-km
# targets). The control phase launches without it, and checks it is absent.
if [[ "$ASSERT_MODE" == "trusted-payment" ]]; then
  tp_flag=add; [[ "$TRUSTED_PAYMENT_PHASE" == "control" ]] && tp_flag=absent
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  python3 - "$ARGS_FILE" "$RENDERED_ARGS" "$tp_flag" <<'PY' || die "trusted-payment: cannot use $ARGS_FILE (see above)"
import re,sys
src,dst,want=sys.argv[1],sys.argv[2],sys.argv[3]
flag="--allowDangerousTrustedPayments"
lines=open(src).read().splitlines(keepends=True)
cl=next((m.group(1) for m in (re.match(r"\s*(?:-\s+)?cl_type:\s*(\S+)",l) for l in lines) if m),None)
if cl!="lodestar":
    sys.exit("participant 1 runs cl_type {0!r}; {1} is a lodestar VC flag".format(cl,flag))
if any(flag in l for l in lines):
    if want=="absent": sys.exit("the args file already passes {0}; the control needs a VC without it".format(flag))
elif want=="add":
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
  if [[ "$tp_flag" == add ]]; then
    grep -q -- '- --allowDangerousTrustedPayments$' "$ARGS_FILE" || die "trusted-payment: flag render failed"
    grep -q -- '- --builder-api-execution-payment-percent=50$' "$ARGS_FILE" || die "trusted-payment: buildoor flag render failed"
    echo "vc flag: --allowDangerousTrustedPayments (rendered -> $ARGS_FILE)"
  else
    echo "vc flag: none (control phase)"
  fi
fi

# manual-config: when the page's call sets a min_bid, the beacon node applies it to
# CB's entry too, and buildoor's builder-API bid is only its subsidy (0.0001 ETH
# by default): every bid CB returns would be floored and the block built from
# buildoor's 0.101 ETH p2p bid. Raise the builder-API subsidy above the page's
# min_bid and the p2p bid, so a block built via CB shows the documented call
# routes the proposer's bid requests through CB.
MANUAL_CONFIG_SUBSIDY_GWEI=200000000
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  # the page's call may set no min_bid: a grep that matches nothing must not end the run
  docs_min_bid="$(docs_builder_config_call "$CB_SHA" | grep -oE '"min_bid": *"[0-9]+"' | grep -oE '[0-9]+' | head -1 || true)"
  (( ${docs_min_bid:-0} < MANUAL_CONFIG_SUBSIDY_GWEI )) \
    || die "manual-config: the page's min_bid $docs_min_bid is not below the builder-API subsidy $MANUAL_CONFIG_SUBSIDY_GWEI"
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  python3 - "$ARGS_FILE" "$RENDERED_ARGS" "--builder-api-subsidy=$MANUAL_CONFIG_SUBSIDY_GWEI" <<'PY' || die "manual-config: cannot use $ARGS_FILE (see above)"
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
  echo "buildoor: --builder-api-subsidy=$MANUAL_CONFIG_SUBSIDY_GWEI, above the page's min_bid ${docs_min_bid:-(none)} (rendered -> $ARGS_FILE)"
fi

# VC_EXTRA_PARAMS (any mode) and config-matrix's buildoor bid, rendered into a copy
# of the args file
cm_args=()
for p in $VC_EXTRA_PARAMS; do cm_args+=(--vc-param "$p"); done
if [[ "$ASSERT_MODE" == "config-matrix" ]]; then
  for a in $CM_BUILDOOR_ARGS; do cm_args+=(--buildoor-arg "$a"); done
fi
if (( ${#cm_args[@]} )); then
  RENDERED_ARGS="$(mktemp "${TMPDIR:-/tmp}/gloas-epbs-args.XXXXXX.yaml")"
  python3 "$REPO_ROOT/scripts/epbs-config-matrix.py" render-args "$ARGS_FILE" "$RENDERED_ARGS" "${cm_args[@]}" \
    || die "cannot add ${cm_args[*]} to $ARGS_FILE"
  ARGS_FILE="$RENDERED_ARGS"
  echo "args:    ${cm_args[*]} (rendered -> $ARGS_FILE)"
fi
if [[ "$ASSERT_MODE" == "config-matrix" ]]; then
  mkdir -p "$CM_OUT"
  cp "$ARGS_FILE" "$CM_OUT/args.yaml"
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

# ---- 3. render CB config + overlay from the live chain -----------------------
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
render_cb_config "$RUN_DIR" "$GEN_TIME" "$VC_KM"
[[ "$ASSERT_MODE" == "manual-config" ]] || km_dry_run "$RUN_DIR"

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

# ---- 5. apply the keymanager builder_config (route VC -> CB) -----------------
if [[ "$TRUSTED_PAYMENT_PHASE" == "control" ]]; then
  assert_trusted_payment_refused
  exit 0
fi
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  write_documented_builder_configs
elif [[ "$ASSERT_MODE" == "dial" ]]; then
  write_dial_builder_configs
elif [[ "$ASSERT_MODE" == "config-matrix" ]]; then
  cm_apply
else
  log "cb-km apply (point 64 validators' builder_config at CB)"
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml"
  echo "apply OK"
fi
# checked before the activation wait: a missing cap fails here, minutes earlier
if [[ "$ASSERT_MODE" == "trusted-payment" ]]; then
  assert_cap_projected
fi

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

# config-matrix records and classifies instead of asserting the builder loop
if [[ "$ASSERT_MODE" == "config-matrix" ]]; then
  cm_report "$start_slot" "$end_slot"
  exit $?
fi

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

echo "  BN -> CB bid calls (window+):     $bid_calls"
echo "  buildoor bids via CB:             $buildoor_bids"
echo "  builder-built blocks on chain:    $n_built  [slots: $built_slots]"
echo "    paid only via execution_payment (value = 0): $n_payment_only"

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

# manual-config: the page's min_bid does not floor buildoor's p2p bid, so a
# builder-built block may have come over p2p rather than through CB
if [[ "$ASSERT_MODE" == "manual-config" ]]; then
  BN_LOG="$(bn_logs)"
  echo "  BN selections via CB / via p2p:   $(grep 'Selected builder block' <<<"$BN_LOG" | grep -c 'bidSource=[^, ]*cb-epbs' || true) / $(grep 'Selected builder block' <<<"$BN_LOG" | grep -c 'bidSource=[^, ]*p2p' || true)"
fi

log "RESULT"
if (( n_built >= MIN_BUILDER_SLOTS && buildoor_bids >= 1 && bid_calls >= 1 && p2p_ok == 1 )); then
  printf '\033[1;32mPASS: %s/%s observed slots builder-built via commit-boost (buildoor)\033[0m\n' \
    "$n_built" "$OBSERVE_SLOTS"
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
  die "loop not satisfied (built=$n_built need>=$MIN_BUILDER_SLOTS, buildoor bids=$buildoor_bids, bid calls=$bid_calls)"
fi
