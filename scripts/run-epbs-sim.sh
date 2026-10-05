#!/usr/bin/env bash
# run-epbs-sim.sh - reproducible ePBS (gloas) + commit-boost + keymanager loop.
#
# Stands up a gloas devnet with buildoor (gloas_fork_epoch 0, minimal, 6s slots),
# lodestar CL+VC with keymanager enabled, adds a commit-boost PBS sidecar as a
# first-class kurtosis enclave service (CB_LAUNCH=service; docker = legacy path),
# runs `cb-km apply` to point every validator's builder_config at commit-boost
# (auth_data=buildoor), then observes the loop and asserts:
#   BN -> CB execution_payload_bid calls, buildoor bids via CB (auction winner),
#   and builder-built blocks on chain (the block's signed_execution_payload_bid
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
#   --assert block-submission  also assert CB DECODES the reveal at POST
#                      /eth/v1/builder/beacon_blocks: the proposer reveals each
#                      builder-built block through CB, which SSZ-decodes the gloas
#                      SignedBeaconBlock and fans it out to buildoor. HARD-fails
#                      unless >=1 reveal got a 2xx AND there were zero 4xx
#                      (decode over-read / non-gloas body) AND zero 5xx AND zero
#                      OffsetOutOfBounds in the lodestar log. Defaults to the
#                      MAINNET preset (PRESET=mainnet): CB is compiled
#                      MainnetEthSpec, so a minimal-preset block over-reads and
#                      400s every reveal (OffsetOutOfBounds); mainnet block sizes
#                      match CB and decode. On mainnet a non-2xx is a real
#                      regression, so none are tolerated.
#   --assert builder-down  after buildoor activates and >=1 builder-built slot is
#                      seen, STOP buildoor and assert the proposer stays live: the
#                      chain keeps advancing, the new blocks are self-built
#                      (builder_index == BUILDER_INDEX_SELF_BUILD), and CB returns
#                      204 "no header available" and NEVER 500. Proves builder
#                      failure never stalls the proposer.
#   --assert request-auth  render CB with verify_builder_request_auth = true and
#                      assert the loop STILL produces builder-built blocks with
#                      ZERO AuthSigVerify / 401 on the bid endpoint. Proves CB's
#                      request-auth signing domain (genesis_fork_version + zero
#                      genesis_validators_root, per builder-specs) agrees with the
#                      real lodestar VC signer end to end. Independent of the bid
#                      skip_sigverify progressive-SSZ blocker (the request-auth
#                      message is a plain container, not a progressive one).
#   --assert direct    apply with `direct_entries = true` in the km overlay, so
#                      each validator also gets a builder_config entry pointing
#                      straight at buildoor (same auth_data as the CB entry).
#                      HARD-fails unless the stored doc holds both entries, the
#                      BN ranks a bid from BOTH CB and buildoor in >=
#                      MIN_DIRECT_SLOTS slots, and after CB is STOPPED the chain
#                      keeps producing builder-built blocks sourced directly
#                      from buildoor (the redundancy the direct entry buys).
#   --assert ladder    turn on timing games for buildoor (frequency_get_header_ms
#                      = LADDER_FREQ_MS, default 300), so CB polls buildoor in a
#                      ladder of rungs and holds the latest valid bid. HARD-fails
#                      unless the default builder-built PASS holds, the ladder ran
#                      at that cadence, it delivered the bid in >= MIN_BUILDER_SLOTS
#                      requests with >= 2 rungs answered each, and CB logged ZERO
#                      ladder errors ("TG: error sending header request" /
#                      "TG: no headers received").
#   --assert trusted-payment  set [pbs] max_execution_payment_gwei (the relays
#                      carry none, so it is the effective cap) and start the
#                      lodestar VC with --allowDangerousTrustedPayments, and tell
#                      buildoor to claim half of each bid as execution_payment
#                      (--builder-api-execution-payment-percent). HARD-fails unless
#                      cb-km stored the cap on every builder_config entry of every
#                      key, the BN selects CB's execution-payment bid in >=
#                      MIN_BUILDER_SLOTS slots, and for each of them the BN's
#                      valuation equals CB's capped ranking of the same bid. First
#                      runs a CONTROL devnet WITHOUT the flag (skip with
#                      TRUSTED_PAYMENT_CONTROL=0): cb-km apply must exit non-zero
#                      with a 400 on every key, the VC naming the flag.
# No flag = the default builder-built assertion (unchanged).
#
# One devnet at a time (~15G). Usage:
#   scripts/run-epbs-sim.sh [--assert p2p|preserve|block-submission|builder-down|request-auth|direct|ladder|trusted-payment]
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
# activates ~slot 33) and is enough for the p2p/preserve/default modes. mainnet =
# REQUIRED for --assert block-submission's /beacon_blocks decode (CB is compiled
# MainnetEthSpec; a minimal-preset block over-reads its decoder and 400s), at the
# cost of 32-slot epochs (buildoor activates ~slot 128) and a ~30-45 min run.
PRESET="${PRESET:-}"
# Empty = provision from the pinned commit-boost-client submodule (sha-tagged,
# never stale; see scripts/ensure-cb-artifacts.sh). Set explicitly to pin a
# different CB image.
CB_IMAGE="${CB_IMAGE:-}"
# Swap the CL/VC image to test a specific consensus client build against the sim
# (e.g. CL_IMAGE=chainsafe/lodestar:v1.47.0-rc.0). Empty = use the args file's.
CL_IMAGE="${CL_IMAGE:-}"
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
  ""|p2p|preserve|block-submission|builder-down|request-auth|direct|ladder|trusted-payment) : ;;
  *) printf 'unknown --assert mode: %s (want p2p|preserve|block-submission|builder-down|request-auth|direct|ladder|trusted-payment)\n' "$ASSERT_MODE" >&2; exit 2 ;;
esac

# block-submission's /beacon_blocks decode only succeeds on the mainnet preset
# (CB is compiled MainnetEthSpec). Default this mode to mainnet unless the caller
# picked a preset explicitly.
if [[ "$ASSERT_MODE" == "block-submission" && -z "$PRESET" ]]; then
  PRESET=mainnet
fi
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
# direct: slots whose ranked candidates must include BOTH paths, and slots to watch
# with CB stopped
MIN_DIRECT_SLOTS="${MIN_DIRECT_SLOTS:-4}"
CB_DOWN_SLOTS="${CB_DOWN_SLOTS:-12}"
# ladder: ms between the timing-games rungs CB sends buildoor
LADDER_FREQ_MS="${LADDER_FREQ_MS:-300}"
# trusted-payment: the [pbs] max_execution_payment_gwei cb-km projects onto every
# entry. The default covers buildoor's whole bid, so buildoor pays only through
# execution_payment and bids value = 0.
TRUSTED_PAYMENT_CAP_GWEI="${TRUSTED_PAYMENT_CAP_GWEI:-1000000000}"
# trusted-payment: 1 = first run the control devnet, whose VC lacks the flag
TRUSTED_PAYMENT_CONTROL="${TRUSTED_PAYMENT_CONTROL:-1}"
# internal: "control" marks the control phase's own run of this script
TRUSTED_PAYMENT_PHASE="${TRUSTED_PAYMENT_PHASE:-}"
[[ "$LADDER_FREQ_MS" =~ ^[1-9][0-9]*$ ]] || { printf 'LADDER_FREQ_MS must be a positive integer: %s\n' "$LADDER_FREQ_MS" >&2; exit 2; }
[[ "$TRUSTED_PAYMENT_CAP_GWEI" =~ ^[1-9][0-9]*$ ]] \
  || { printf 'TRUSTED_PAYMENT_CAP_GWEI must be a positive integer: %s\n' "$TRUSTED_PAYMENT_CAP_GWEI" >&2; exit 2; }

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

# --- assert block-submission: CB SSZ-decodes the CL's gloas SignedBeaconBlock ------
# The proposer reveals every builder-built (CB-won) block by POSTing the gloas
# SignedBeaconBlock to its builder URL (CB) at POST /eth/v1/builder/beacon_blocks.
# block-submission mode enables strict_block_decode, so CB parses the SSZ body ITSELF
# (default OFF = blind pipe, forwards the bytes unparsed) before fanning it out.
#
# Decode runs BEFORE the fan-out, and the two stages map onto CB's status cleanly:
#   4xx -> decode/request REJECT: an SSZ over-read (OffsetOutOfBounds/DecodeError) or
#          a non-gloas body (NotGloasBlock -> 400). This is the preset-mismatch
#          failure: CB is compiled MainnetEthSpec, so a minimal-preset block
#          over-reads and 400s every reveal; on the mainnet preset the sizes match
#          and it decodes (hence block-submission defaults PRESET=mainnet).
#   5xx -> decode SUCCEEDED, then no builder accepted the HTTP forward
#          (NoBuilderResponse -> 500). EXPECTED with buildoor, which reveals over p2p
#          and does not implement the /beacon_blocks forward; the decode still ran.
#   2xx -> decoded AND a builder accepted the forward (a relay that implements it).
# So "CB decoded" == it reached the fan-out == a 2xx or a 5xx, corroborated by CB's
# post-decode access line ("new request ... strict=true slot=Some(N)"), logged only
# once the SSZ body parsed. A blind-pipe run also reaches the fan-out WITHOUT
# decoding, so the PASS additionally requires strict_block_decode = true in the
# rendered config - otherwise a forward would not prove a decode.
# PASS: strict on AND >=1 block reached the fan-out AND zero 4xx AND zero
# OffsetOutOfBounds. Forward 5xx are reported, not failed (buildoor never HTTP-ACKs).
assert_block_submission() {
  log "assert block-submission: CB SSZ-decodes the CL's gloas SignedBeaconBlock at /beacon_blocks (strict_block_decode)"
  local CB_LOG BN_LOG bb fwd_2xx decode_fail no_builder offset decoded_log reached strict_on
  strict_on=$(grep -c '^strict_block_decode = true$' "$RUN_DIR/cb-config.toml" || true)
  CB_LOG="$(cb_logs | sed -r 's/\x1b\[[0-9;]*m//g')"   # strip ANSI so the status parses
  BN_LOG="$(bn_logs)"
  bb="$(grep 'method=/eth/v1/builder/beacon_blocks' <<<"$CB_LOG" | grep -oE 'Responded with [0-9]{3}')"
  fwd_2xx=$(grep -c 'Responded with 2' <<<"$bb" || true)
  decode_fail=$(grep -c 'Responded with 4' <<<"$bb" || true)
  no_builder=$(grep -c 'Responded with 5' <<<"$bb" || true)
  # decode is BEFORE the fan-out, so a forward (2xx or 5xx NoBuilderResponse) proves
  # the SSZ body parsed - PROVIDED strict is on (a blind pipe forwards unparsed).
  reached=$(( fwd_2xx + no_builder ))
  # corroboration: the post-decode access line, emitted only after the body parsed
  decoded_log=$(grep 'new request' <<<"$CB_LOG" | grep -c 'slot=Some(' || true)
  offset=$(grep -cE 'OffsetOutOfBounds' <<<"$CB_LOG"$'\n'"$BN_LOG" || true)
  echo "  strict_block_decode enabled:                    $strict_on (need 1)"
  echo "  reached fan-out = decoded (2xx+5xx):            $reached"
  echo "  corroborating 'slot=Some' decode log lines:     $decoded_log"
  echo "  /beacon_blocks 2xx (decoded + forwarded):       $fwd_2xx"
  echo "  /beacon_blocks 5xx (decoded, buildoor p2p - ok):$no_builder"
  echo "  /beacon_blocks 4xx (decode/request REJECT):     $decode_fail"
  echo "  OffsetOutOfBounds decode failures (CB+CL):      $offset"
  if (( strict_on >= 1 && reached >= 1 && decode_fail == 0 && offset == 0 )); then
    printf '\033[1;32mPASS: CB decoded %s gloas SignedBeaconBlock(s) (0 4xx, 0 OffsetOutOfBounds; %s forward 5xx = buildoor p2p, expected)\033[0m\n' "$reached" "$no_builder"
    return 0
  fi
  echo "  --- CB /beacon_blocks access lines (tail) ---"
  grep 'method=/eth/v1/builder/beacon_blocks' <<<"$CB_LOG" | tail -10 | sed 's/^/    /'
  echo "  --- CL submit-to-builder failures (tail) ---"
  grep -iE 'submit signed block to builder|OffsetOutOfBounds|status=4' <<<"$BN_LOG" | tail -6 | sed 's/^/    /'
  die "block-submission failed (strict=$strict_on need1, decoded=$reached need>=1, 4xx=$decode_fail need0, OffsetOutOfBounds=$offset need0)"
}

# --- assert request-auth: builder loop survives verify_builder_request_auth ------
# The config was rendered with verify_builder_request_auth = true. CB now checks
# the caller's SignedBuilderRequestAuth signature against the proposer pubkey on
# every bid request. If CB's request-auth domain disagrees with what the lodestar
# VC signed, CB returns 401 (AuthSigVerify) and the loop dies. PASS = builder-built
# blocks still land AND zero AuthSigVerify/401. A 401 is a REAL domain-mismatch
# finding: dump the domain params CB used.
assert_request_auth() { # $1=n_built
  log "assert request-auth: builder loop survives verify_builder_request_auth = true"
  local n_built="$1"
  local CB_LOG; CB_LOG="$(cb_logs)"
  local authfail
  authfail=$(grep -c -E 'auth signature verification failed|AuthSigVerify' <<<"$CB_LOG" || true)
  echo "  builder-built blocks on chain (verify on):  $n_built"
  echo "  AuthSigVerify / 401 on the bid endpoint:    $authfail"
  if (( n_built >= MIN_BUILDER_SLOTS && authfail == 0 )); then
    printf '\033[1;32mPASS: request-auth verify ON, %s builder-built slots, 0 AuthSigVerify - CB domain agrees with lodestar\033[0m\n' "$n_built"
    return 0
  fi
  if (( authfail > 0 )); then
    printf '\033[1;31mFINDING: request-auth signature verification FAILED (%s x 401) - CB domain disagrees with lodestar VC\033[0m\n' "$authfail"
    echo "  CB request-auth domain = compute_domain(DOMAIN_BUILDER_REQUEST_AUTH):"
    echo "    genesis_fork_version = $(grep -oE 'genesis_fork_version = \"0x[0-9a-fA-F]+\"' "$RUN_DIR/cb-config.toml" | head -1)"
    echo "    genesis_validators_root = 0x0000..0000 (zero root, per builder-specs request_auth)"
    echo "  live BN GENESIS_FORK_VERSION = $(curl -sf "$BN/eth/v1/config/spec" | python3 -c "import sys,json;print(json.load(sys.stdin)['data'].get('GENESIS_FORK_VERSION'))" 2>/dev/null || echo '?')"
    echo "  live BN genesis_validators_root = $(curl -sf "$BN/eth/v1/beacon/genesis" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['genesis_validators_root'])" 2>/dev/null || echo '?')"
    cb_logs | grep -E 'auth signature|AuthSigVerify|get_execution_payload_bid failed' | tail -20
  fi
  die "request-auth failed (built=$n_built need>=$MIN_BUILDER_SLOTS, authfail=$authfail need=0)"
}

# --- assert direct: the BN reaches buildoor through CB AND directly --------------
# Lodestar asks every builder_config entry for a bid in parallel and logs one
# "Ranked builder bid candidates slot=N, candidates=<url>:total=..,<url>:..,
# bidSource=<url>" line per proposal, so both paths are visible per slot.
DIRECT_URL="$(sed -E 's|://[^@/]*@|://|' <<<"$BUILDOOR_RELAY_URL")"
CB_ENTRY_URL="http://${CB_NAME}:18550"

# ranked lines for slots in [$1,$2] -> "slot cb direct bidSource" rows
ranked_rows() {
  bn_logs | grep 'Ranked builder bid candidates' | python3 -c '
import re,sys
s0,s1,cb,direct=int(sys.argv[1]),int(sys.argv[2]),sys.argv[3],sys.argv[4]
for line in sys.stdin:
    m=re.search(r"slot=(\d+)",line)
    if not m or not s0<=int(m.group(1))<=s1: continue
    cands=re.search(r"candidates=(.*?), bidSource=",line)
    cands=cands.group(1) if cands else ""
    src=re.search(r"bidSource=(\S+)",line)
    print(m.group(1), int(cb+":" in cands), int(direct+":" in cands), src.group(1) if src else "-")
' "$1" "$2" "$CB_ENTRY_URL" "$DIRECT_URL"
}

stop_cb() {
  if [[ "$CB_LAUNCH" == "service" ]]; then
    kurtosis service stop "$ENCLAVE" "$CB_NAME" >/dev/null 2>&1
  else
    docker stop "$CB_NAME" >/dev/null 2>&1
  fi
}

assert_direct() { # $1,$2 = observed window
  log "assert direct: builder_config holds a CB entry and a direct buildoor entry"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  local K1; K1="$(curl -sf -H "Authorization: Bearer $KM_TOKEN" "$VC_KM/eth/v1/keystores" | python3 -c 'import sys,json;print(json.load(sys.stdin)["data"][0]["validating_pubkey"])')"
  local urls; urls="$(km_entry_urls "$K1")"
  echo "  $K1 entries: $(tr '\n' ' ' <<<"$urls")"
  grep -qxF "$CB_ENTRY_URL" <<<"$urls" || die "direct: no CB entry ($CB_ENTRY_URL) stored"
  grep -qxF "$DIRECT_URL" <<<"$urls" || die "direct: no direct entry ($DIRECT_URL) stored"

  log "assert direct: the BN ranks bids from both paths in the same slot"
  local rows; rows="$(ranked_rows "$1" "$2")"
  local both; both=$(awk '$2==1 && $3==1' <<<"$rows" | wc -l)
  echo "  ranked slots in [$1,$2]: $(wc -l <<<"$rows"), with both CB and direct candidates: $both"
  grep -m3 'Ranked builder bid candidates' <<<"$(bn_logs)" | sed 's/^/    e.g. /' | cut -c1-300
  (( both >= MIN_DIRECT_SLOTS )) || { echo "$rows"; die "direct: only $both slot(s) ranked both paths (need >=$MIN_DIRECT_SLOTS)"; }

  log "assert direct: stop CB, blocks stay builder-built through the direct entry"
  local stop_slot; stop_slot="$(bn_head_slot)"
  stop_cb || die "could not stop $CB_NAME"
  local target=$(( stop_slot + CB_DOWN_SLOTS )) cur=0
  for i in $(seq 1 $(( CB_DOWN_SLOTS + 4 )) ); do
    cur="$(bn_head_slot)"
    printf '  head=%s / target=%s (CB down)\r' "$cur" "$target"
    (( cur >= target )) && break
    sleep 6
  done
  echo
  local from=$(( stop_slot + 2 ))
  local built_after; built_after="$(sed -n 's/^builder=//p' <<<"$(classify_blocks "$from" "$cur")")"
  local n_built_after=0
  [[ -n "$built_after" ]] && n_built_after=$(awk -F, '{print NF}' <<<"$built_after")
  local after; after="$(ranked_rows "$from" "$cur")"
  local direct_won; direct_won=$(awk -v u="$DIRECT_URL" '$4==u' <<<"$after" | wc -l)
  local cb_seen; cb_seen=$(awk '$2==1' <<<"$after" | wc -l)
  echo "  head advanced $(( cur - stop_slot )) slots with CB stopped"
  echo "  builder-built blocks after stop: $n_built_after  [slots: $built_after]"
  echo "  selections won by the direct entry: $direct_won; CB candidates after stop: $cb_seen"

  log "RESULT"
  if (( n_built_after >= 1 && direct_won >= 1 && cb_seen == 0 )); then
    printf '\033[1;32mPASS: both paths ranked in %s slots; with CB down, %s blocks built via the direct entry\033[0m\n' \
      "$both" "$n_built_after"
    return 0
  fi
  echo "$after"
  die "direct: CB down -> built=$n_built_after need>=1, direct wins=$direct_won need>=1, CB candidates=$cb_seen need=0"
}

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
  cb_204=$(grep -c 'no header available for slot' <<<"$CB_TAIL" || true)
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

# --- assert ladder: the timing-games ladder delivers the bid, error-free ---------
# With timing games on, CB asks buildoor for a bid every LADDER_FREQ_MS until the
# proposer's deadline and holds the LATEST valid answer. The ladder lines are
# debug-level, which the template enables ([logs.stdout] level = "debug").
# PASS: the ladder ran at the configured cadence on the relay the keys route to
# (buildoor-mux), delivered the bid in >= MIN_BUILDER_SLOTS requests with >= 2
# rungs answered in each (a ladder, not one request), and CB logged zero ladder
# errors: "TG: error sending header request" (a rung failed with something other
# than a timeout) or "TG: no headers received" (every rung failed).
assert_ladder() {
  log "assert ladder: timing games at ${LADDER_FREQ_MS}ms deliver buildoor's bid with no ladder errors"
  local report rc=0
  report="$(cb_logs | sed -r 's/\x1b\[[0-9;]*m//g' | python3 -c '
import re,sys
freq,need=int(sys.argv[1]),int(sys.argv[2])
relay="relay_id=\"buildoor-mux\""
runs,other_freq,held,headers,errors=0,0,set(),{},[]
for line in sys.stdin:
    req=re.search(r"\breq_id=(\S+)",line)
    req=req.group(1) if req else None
    if "TG: sending multiple header requests" in line and relay in line:
        f=re.search(r"\bsend_freq_ms=(\d+)",line)
        if f and int(f.group(1))==freq: runs+=1
        else: other_freq+=1
    elif "TG: holding the relay" in line and relay in line and req:
        held.add(req)
    elif "received new header" in line and relay in line and req:
        headers[req]=headers.get(req,0)+1
    elif "TG: error sending header request" in line or "TG: no headers received" in line:
        errors.append(line.strip())
multi=sorted(headers.get(r,0) for r in held if headers.get(r,0)>=2)
print("  ladder runs at send_freq_ms={0}:            {1} (other cadence: {2})".format(freq,runs,other_freq))
print("  requests whose ladder delivered the bid:   {0} (need >= {1})".format(len(held),need))
print("    of which with >= 2 rungs answered:       {0} (rungs answered: min {1}, max {2})".format(
    len(multi),multi[0] if multi else 0,multi[-1] if multi else 0))
print("  ladder errors (TG error / no headers):     {0} (need 0)".format(len(errors)))
for e in errors[:5]: print("    "+e[:240])
ok=runs>=1 and other_freq==0 and len(held)>=need and len(multi)>=need and not errors
print("{0} ladder bids delivered, {1} with >= 2 rungs, at {2}ms, {3} ladder errors".format(
    len(held),len(multi),freq,len(errors)))
sys.exit(0 if ok else 1)
' "$LADDER_FREQ_MS" "$MIN_BUILDER_SLOTS")" || rc=$?
  sed '$d' <<<"$report"
  (( rc == 0 )) || die "ladder: $(tail -1 <<<"$report")"
  printf '\033[1;32mPASS: %s\033[0m\n' "$(tail -1 <<<"$report")"
}

# --- assert trusted-payment, control: a VC without the flag refuses the cap ------
# Run in the control phase's own devnet, whose lodestar VC lacks
# --allowDangerousTrustedPayments. lodestar's keymanager refuses any
# builder_config entry with max_execution_payment > 0 (400, naming the flag), so
# cb-km apply of the capped config must exit non-zero with a 400 on every key,
# and no key may end up holding the cap. A direct POST of one projected doc ties
# the 400 to the flag, not to some other rejection.
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
  # || true: no match is a 0 here, not a reason for set -e to end the run
  refused="$(grep -E 'POST 0x[0-9a-f]{96} failed: .*failed: 400' <<<"$out" \
    | grep -oE 'POST 0x[0-9a-f]{96}' | sort -u | wc -l || true)"
  echo "  cb-km apply exit code:            $rc (need non-zero)"
  echo "  keys refused with HTTP 400:       $refused of $n_keys (need all)"
  { grep -m1 -E 'failed: 400' <<<"$out" || true; } | cut -c1-240 | sed 's/^/    e.g. /'

  # one projected doc, POSTed by hand to read the VC's reason
  local emit_dir="$RUN_DIR/emitted" key doc code body stored_cap
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" --emit "$emit_dir" >/dev/null 2>&1 \
    || die "control: cb-km apply --emit failed"
  read -r key doc < <(python3 -c 'import json,sys;k=json.load(open(sys.argv[1]))["keys"][0];print(k["pubkey"],k["file"])' "$emit_dir/manifest.json") \
    || die "control: no projected doc in $emit_dir/manifest.json"
  body="$(curl -s -w '\n%{http_code}' -X POST -H "Authorization: Bearer $KM_TOKEN" -H 'Content-Type: application/json' \
    --data @"$emit_dir/$doc" "$VC_KM/eth/v1/validator/$key/builder_config")"
  code="$(tail -1 <<<"$body")"; body="$(sed '$d' <<<"$body")"
  stored_cap="$(km_get "$key" | python3 -c \
    'import sys,json;d=json.load(sys.stdin)["data"];print(max([int(e.get("max_execution_payment") or 0) for e in d.get("builders") or []],default=0))')" \
    || die "control: could not read $key's stored builder_config"
  echo "  direct POST of $key's projected doc: HTTP $code $body"
  echo "  largest cap stored on that key:   $stored_cap (need 0)"

  log "RESULT"
  if (( rc != 0 && n_keys >= 1 && refused == n_keys )) && [[ "$code" == 400 && "$body" == *allowDangerousTrustedPayments* && "$stored_cap" == 0 ]]; then
    printf '\033[1;32mPASS: without --allowDangerousTrustedPayments, cb-km apply exited %s with a 400 on all %s keys and stored no cap\033[0m\n' "$rc" "$n_keys"
    return 0
  fi
  die "control: want cb-km exit != 0 (got $rc), 400 on all $n_keys keys (got $refused), a direct 400 naming the flag (got $code), stored cap 0 (got $stored_cap)"
}

# --- assert trusted-payment: cb-km stored the cap on every entry -------------------
# The relays carry no max_execution_payment_gwei, so [pbs] holds the effective cap
# and cb-km must project it onto every builder_config entry of every key (a
# relay-only projection leaves the entries uncapped, so the BN values the trusted
# payment at 0 while CB ranks it in full).
assert_cap_projected() {
  log "assert trusted-payment: cb-km stored max_execution_payment = $TRUSTED_PAYMENT_CAP_GWEI on every entry"
  KM_TOKEN="$(cat "$RUN_DIR/km-token.txt")"
  python3 - "$VC_KM" "$KM_TOKEN" "$TRUSTED_PAYMENT_CAP_GWEI" <<'PY' || die "trusted-payment: the cap is missing from stored builder_config entries"
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

# --- assert trusted-payment: the BN selects CB's bid and values it as CB ranks it --
# CB ranks a bid at value + min(execution_payment, cap) with cap = the relay's
# max_execution_payment_gwei, else [pbs]'s (beacon-APIs #630); the BN values the
# same bid with the cap stored on the builder_config entry. For each slot in the
# window where the BN selected CB's bid ("Selected builder block ... bidSource=
# <CB>"), CB's auction winner must be that bid (same value and
# execution_payment) and CB's capped ranking of it must equal the BN's total for
# the CB candidate ("Ranked builder bid candidates ... <CB>:total=N", debug).
# buildoor splits its bid at the cap it is told, so with buildoor the cap never
# clamps CB's ranking: this proves CB and the BN agree on the cap and the formula,
# not the clamp itself. PASS: >= MIN_BUILDER_SLOTS CB selections carry
# execution_payment > 0 and no selected slot mismatches or lacks either side.
assert_trusted_payment() { # $1,$2 = observed window
  log "assert trusted-payment: the BN selects CB's execution-payment bid and values it as CB ranks it"
  cb_logs | sed -r 's/\x1b\[[0-9;]*m//g' > "$RUN_DIR/cb.log"
  bn_logs | sed -r 's/\x1b\[[0-9;]*m//g' > "$RUN_DIR/bn.log"
  local report rc=0
  report="$(python3 - "$RUN_DIR/cb.log" "$RUN_DIR/bn.log" "$1" "$2" "$CB_ENTRY_URL" \
      "$TRUSTED_PAYMENT_CAP_GWEI" "$MIN_BUILDER_SLOTS" <<'PY'
import re,sys
from decimal import Decimal
cb_log,bn_log=sys.argv[1],sys.argv[2]
s0,s1,cb_url,cap,need=int(sys.argv[3]),int(sys.argv[4]),sys.argv[5],int(sys.argv[6]),int(sys.argv[7])
def gwei(eth): return int(Decimal(eth)*10**9)
cb={}      # slot -> (value, execution_payment) of CB's auction winner, gwei
for line in open(cb_log,errors="replace"):
    if "auction winner" not in line or 'relay_id="buildoor-mux"' not in line: continue
    slot=re.search(r"\bslot=(\d+)",line)
    v=re.search(r'trustless_bid_eth="([0-9.]+)"',line)
    ep=re.search(r'execution_payment_eth="([0-9.]+)"',line)
    if slot and v and ep: cb[int(slot.group(1))]=(gwei(v.group(1)),gwei(ep.group(1)))
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
        problems.append("slot {0}: the BN selected CB's bid but CB logged no auction winner".format(slot)); continue
    if cb[slot]!=selected[slot]:
        problems.append("slot {0}: CB's winner value/execution_payment {1} != the BN's selected {2}".format(slot,cb[slot],selected[slot])); continue
    rank=cb[slot][0]+min(cb[slot][1],cap)
    if slot not in ranked:
        problems.append("slot {0}: no 'Ranked builder bid candidates' total for {1}".format(slot,cb_url)); continue
    if ranked[slot]!=rank:
        problems.append("slot {0}: CB capped ranking {1} != BN valuation {2}".format(slot,rank,ranked[slot])); continue
    agree.append(slot)
print("  BN selected CB's bid in [{0},{1}]:           {2} slots".format(s0,s1,len(selected)))
print("    with execution_payment > 0:               {0} (need >= {1})".format(len(with_ep),need))
print("  CB capped ranking == BN valuation:          {0} of {1}".format(len(agree),len(selected)))
if agree:
    s=agree[0]; v,ep=cb[s]
    print("    e.g. slot {0}: CB value={1} execution_payment={2} cap={3} -> ranking {4}; BN total={5}".format(
        s,v,ep,cap,v+min(ep,cap),ranked[s]))
for p in problems[:8]: print("    "+p)
ok=len(with_ep)>=need and not problems
print("the BN selected CB's execution-payment bid in {0} slots; CB's capped ranking equals the BN's valuation in {1} of {2}".format(
    len(with_ep),len(agree),len(selected)))
sys.exit(0 if ok else 1)
PY
)" || rc=$?
  sed '$d' <<<"$report"
  (( rc == 0 )) || die "trusted-payment: $(tail -1 <<<"$report")"
  printf '\033[1;32mPASS: %s\033[0m\n' "$(tail -1 <<<"$report")"
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
# Provision the ePBS CB artifacts (image + cb-km) from the pinned submodule,
# sha-tagged so a stale build can never be silently used. Skipped when the caller
# pins CB_IMAGE explicitly (advanced path; CB_KM_BIN then falls back to discovery).
if [[ -z "$CB_IMAGE" ]]; then
  # Capture stdout (the two eval-able lines); the script streams build output to
  # stderr live. Hard-fail on a build error rather than silently continuing with
  # an empty CB_IMAGE (the "silence == success" trap).
  cb_art="$(bash "$(dirname "$0")/ensure-cb-artifacts.sh")" \
    || die "provisioning CB artifacts from the submodule failed (see build output above)"
  km_override="$CB_KM_BIN"
  eval "$cb_art"   # sets CB_IMAGE + CB_KM_BIN
  # a caller's CB_KM_BIN (e.g. a cb-km branch build) wins over the submodule's
  [[ -n "$km_override" ]] && CB_KM_BIN="$km_override"
fi
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
# Every builder-built count below rests on this classifier; check it against its
# boundary cases before spending a devnet on it.
python3 scripts/epbs-blocks.py self-test || die "scripts/epbs-blocks.py self-test failed"
avail_g=$(free -g | awk '/^Mem:/{print $7}')
(( avail_g >= 10 )) || echo "WARN: only ${avail_g}G RAM available; a devnet wants ~15G"
echo "cb-km:   $CB_KM_BIN"
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
# preset is a pure runtime knob (minimal = fast default; mainnet = block-submission).
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

# clean any stale run (docker-path container is a separate object; the enclave rm
# clears a service-path CB with the rest of the enclave)
docker rm -f "$CB_NAME" >/dev/null 2>&1 || true
kurtosis enclave rm -f "$ENCLAVE" >/dev/null 2>&1 || true

# ---- 1. launch the gloas devnet ---------------------------------------------
log "launching gloas devnet ($ARGS_FILE)"
kurtosis run "$EP_PACKAGE" --enclave "$ENCLAVE" --args-file "$ARGS_FILE" --image-download always

NET="kt-${ENCLAVE}"
# Discover the CL/VC service names (client-agnostic: cl-1-<cl>-<el>, vc-1-<el>-<cl>),
# so the loop works for lodestar, prysm, etc. without hardcoding the client.
BN_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE 'cl-1-[a-z0-9-]+' | head -1)"
VC_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE 'vc-1-[a-z0-9-]+' | head -1)"
[[ -n "$BN_SVC" && -n "$VC_SVC" ]] || die "could not discover cl-1-* / vc-1-* services in $ENCLAVE"
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
GEN_TIME=$(curl -sf "$BN/eth/v1/beacon/genesis" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['genesis_time'])")
# Strict-shape it before it reaches sed: it interpolates into a config the CB
# process trusts, so a malformed BN response must fail loud, not inject.
[[ "$GEN_TIME" =~ ^[0-9]+$ ]] || die "genesis_time is not a plain integer: '$GEN_TIME'"
echo "genesis_time=$GEN_TIME"

rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
# km-token.txt is the well-known static ethereum-package keymanager token (public,
# sim-only). It is NOT a secret and must never be reused against a real VC. See docs/EPBS.md.
cp configs/epbs/km-token.txt "$RUN_DIR/km-token.txt"
sed -e "s|__GENESIS_TIME__|$GEN_TIME|" \
    -e "s|__BUILDOOR_RELAY_URL__|$BUILDOOR_RELAY_URL|g" \
    configs/epbs/cb-config.toml.tmpl > "$RUN_DIR/cb-config.toml"
sed -e "s|__VC_KM_URL__|$VC_KM|" \
    -e "s|__TOKEN_PATH__|$(pwd)/$RUN_DIR/km-token.txt|" \
    configs/epbs/km-overlay.toml.tmpl > "$RUN_DIR/km-overlay.toml"
grep -q '__' "$RUN_DIR/cb-config.toml" && die "unrendered placeholder in cb-config.toml"

# request-auth mode: turn ON the request-auth signature check (default off in the
# template). CB verifies the caller's SignedBuilderRequestAuth against the
# proposer pubkey using the builder-specs request-auth domain
# (compute_domain(DOMAIN_BUILDER_REQUEST_AUTH) = genesis_fork_version + zero root).
# This is orthogonal to skip_sigverify (that gates the BID signature, which the
# devnet's progressive-SSZ hashing still blocks); the request-auth message is a
# plain container, so its root agrees across clients.
if [[ "$ASSERT_MODE" == "request-auth" ]]; then
  sed -i 's|^skip_sigverify = .*$|&\nverify_builder_request_auth = true|' "$RUN_DIR/cb-config.toml"
  grep -q '^verify_builder_request_auth = true$' "$RUN_DIR/cb-config.toml" \
    || die "request-auth: failed to enable verify_builder_request_auth in cb-config.toml"
  echo "request-auth: verify_builder_request_auth = true"
fi

# block-submission mode: turn ON strict_block_decode so CB SSZ-decodes the revealed
# gloas SignedBeaconBlock ITSELF (default OFF = blind pipe: it forwards the body
# unparsed and block validity is the builder's job). This is the code path under
# test; without it the decode never runs. The decode happens BEFORE the fan-out to
# builders, so it is exercised even though buildoor rejects the HTTP forward (it
# reveals the payload over p2p, not via /beacon_blocks).
if [[ "$ASSERT_MODE" == "block-submission" ]]; then
  sed -i 's|^skip_sigverify = .*$|&\nstrict_block_decode = true|' "$RUN_DIR/cb-config.toml"
  grep -q '^strict_block_decode = true$' "$RUN_DIR/cb-config.toml" \
    || die "block-submission: failed to enable strict_block_decode in cb-config.toml"
  echo "block-submission: strict_block_decode = true"
fi

# ladder mode: timing games on both buildoor relay entries (the mux entry is the
# one the 64 keys route to), so CB polls buildoor every LADDER_FREQ_MS until the
# proposer's deadline and holds the latest valid bid.
if [[ "$ASSERT_MODE" == "ladder" ]]; then
  sed -i -E 's|^id = "buildoor(-mux)?"$|&\nenable_timing_games = true\nfrequency_get_header_ms = '"$LADDER_FREQ_MS"'|' "$RUN_DIR/cb-config.toml"
  [[ "$(grep -c "^frequency_get_header_ms = $LADDER_FREQ_MS\$" "$RUN_DIR/cb-config.toml")" == 2 ]] \
    || die "ladder: failed to enable timing games on both buildoor relay entries in cb-config.toml"
  echo "ladder: enable_timing_games = true, frequency_get_header_ms = $LADDER_FREQ_MS on both relay entries"
fi

# trusted-payment mode: the [pbs] cap is the only max_execution_payment_gwei in
# the file, so it is the effective cap for both relay entries; cb-km projects it
# onto every builder_config entry and CB ranks bids with it.
if [[ "$ASSERT_MODE" == "trusted-payment" ]]; then
  sed -i "s|^skip_sigverify = .*$|&\nmax_execution_payment_gwei = $TRUSTED_PAYMENT_CAP_GWEI|" "$RUN_DIR/cb-config.toml"
  [[ "$(grep -c '^max_execution_payment_gwei = ' "$RUN_DIR/cb-config.toml")" == 1 ]] \
    && grep -q "^max_execution_payment_gwei = $TRUSTED_PAYMENT_CAP_GWEI\$" "$RUN_DIR/cb-config.toml" \
    || die "trusted-payment: want exactly one max_execution_payment_gwei ([pbs]) in cb-config.toml"
  echo "trusted-payment: [pbs] max_execution_payment_gwei = $TRUSTED_PAYMENT_CAP_GWEI (no relay-level cap)"
fi

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
# CB WARN-logs each [pbs] or mux key it does not know and then ignores it, so a
# stale or misspelled template key would change nothing while reading as set.
unknown_fields="$(cb_logs | sed -r 's/\x1b\[[0-9;]*m//g' | grep -E 'unknown field .* is ignored' || true)"
[[ -z "$unknown_fields" ]] \
  || { echo "$unknown_fields"; die "CB ignores config keys it does not know (fix configs/epbs/cb-config.toml.tmpl)"; }
echo "CB config: no unknown keys"

# ---- 5. apply the keymanager builder_config (route VC -> CB) -----------------
# top-level key: it must precede the [[vcs]] table or TOML scopes it to that table
[[ "$ASSERT_MODE" == "direct" ]] && sed -i '1i direct_entries = true' "$RUN_DIR/km-overlay.toml"
if [[ "$TRUSTED_PAYMENT_PHASE" == "control" ]]; then
  assert_trusted_payment_refused
  exit 0
fi
log "cb-km apply (point 64 validators' builder_config at CB)"
"$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml"
echo "apply OK"
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

# chain-side: builder-built blocks (the bid names a registry builder)
classified="$(classify_blocks "$start_slot" "$end_slot")"
built_slots="$(sed -n 's/^builder=//p' <<<"$classified")"
payment_only_slots="$(sed -n 's/^payment_only=//p' <<<"$classified")"
n_built=$(count_csv "$built_slots")
n_payment_only=$(count_csv "$payment_only_slots")

echo "  BN -> CB bid calls (window+):     $bid_calls"
echo "  buildoor auction wins via CB:     $auction_wins"
echo "  builder-built blocks on chain:    $n_built  [slots: $built_slots]"
echo "    paid only via execution_payment (value = 0): $n_payment_only"

# builder-down diverges here: it only needs the builder PROVEN live (>=1
# builder-built slot), then stops buildoor and asserts proposer liveness.
if [[ "$ASSERT_MODE" == "builder-down" ]]; then
  (( n_built >= 1 && auction_wins >= 1 )) \
    || { cb_logs | tail -20; die "builder-down precondition: buildoor never built a block (built=$n_built, wins=$auction_wins)"; }
  echo "  precondition OK: buildoor built >=1 block before stop"
  assert_builder_down "$start_slot"
  exit 0
fi

if [[ "$ASSERT_MODE" == "direct" ]]; then
  assert_direct "$start_slot" "$end_slot"
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

log "RESULT"
if (( n_built >= MIN_BUILDER_SLOTS && auction_wins >= 1 && bid_calls >= 1 && p2p_ok == 1 )); then
  printf '\033[1;32mPASS: %s/%s observed slots builder-built via commit-boost (buildoor)\033[0m\n' \
    "$n_built" "$OBSERVE_SLOTS"
  if [[ "$ASSERT_MODE" == "p2p" ]]; then
    printf '\033[1;32mPASS: p2p floor PROVEN (%s rejected); %s blocks selected CB, 0 selected p2p\033[0m\n' \
      "$p2p_rejected" "$cb_selected"
  fi
  # request-auth: the loop passed with verify_builder_request_auth ON, so also
  # assert zero AuthSigVerify. block-submission: also assert the reveal endpoint fired.
  [[ "$ASSERT_MODE" == "request-auth" ]]     && { assert_request_auth "$n_built"; }
  [[ "$ASSERT_MODE" == "block-submission" ]] && { assert_block_submission; }
  [[ "$ASSERT_MODE" == "ladder" ]]           && { assert_ladder; }
  [[ "$ASSERT_MODE" == "trusted-payment" ]]  && { assert_trusted_payment "$start_slot" "$end_slot"; }
  exit 0
else
  cb_logs | tail -30
  (( p2p_ok == 1 )) || die "p2p assertion failed (rejected=$p2p_rejected need>=1, cb_selected=$cb_selected need>=1, p2p_selected=$p2p_selected need=0)"
  # request-auth: the loop dying WHILE verify is on is the domain-mismatch finding.
  [[ "$ASSERT_MODE" == "request-auth" ]] && assert_request_auth "$n_built"
  die "loop not satisfied (built=$n_built need>=$MIN_BUILDER_SLOTS, wins=$auction_wins, bids=$bid_calls)"
fi
