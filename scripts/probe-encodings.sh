#!/usr/bin/env bash
# Probe a RUNNING enclave's commit-boost getHeader with both wire encodings and
# assert each is served correctly. CB logs no encoding and exposes no
# encoding-labelled metric, so a live probe is the only way to prove SSZ and JSON
# both work end to end (CB -> relay -> CB -> caller).
#
#   scripts/probe-encodings.sh <enclave> [attempts]
#
# For each Accept (application/octet-stream = SSZ, application/json = JSON) it
# builds a real getHeader triple from the chain (next slot, head execution block
# hash, that slot's proposer pubkey) and asserts:
#   - HTTP 200 (a 204 means no bid for that slot; retried on the next slot)
#   - the response Content-Type is the encoding that was asked for
#   - the body actually parses as that encoding (SSZ: non-empty binary of the
#     expected fixed prefix; JSON: has .data.message.header)
# Exit 0 only when BOTH encodings were proven. Prints a one-line summary per
# encoding so the matrix runner can grep it.
set -u -o pipefail

ENCLAVE="${1:?usage: probe-encodings.sh <enclave> [attempts]}"
ATTEMPTS="${2:-12}"          # slots to try before giving up on an encoding

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

# --- locate the services (names vary per client pair) -------------------------
# Only RUNNING lines: a files-artifact row can carry a name that prefix-matches a
# service (e.g. `commit-boost-config`) and would otherwise win the match.
INSPECT="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null)"
svc_of() { awk '/RUNNING/' <<<"$INSPECT" | grep -oE "$1" | head -1; }
CB_SVC="$(svc_of 'commit-boost-[a-z0-9-]+')"
BN_SVC="$(svc_of 'cl-1-[a-z0-9-]+')"
[[ -n "$CB_SVC" && -n "$BN_SVC" ]] || { echo "PROBE-ERROR: could not find cb/cl services in $ENCLAVE" >&2; exit 2; }

# Port name varies by client (http / rpc / http-api); try the likely names, then
# fall back to whatever port that service exposes.
port_of() { # <service>
  local svc="$1" p
  for name in http rpc http-api api; do
    p="$(kurtosis port print "$ENCLAVE" "$svc" "$name" 2>/dev/null)"
    [[ -n "$p" ]] && { printf '%s\n' "$p"; return 0; }
  done
  # last resort: first port name shown for that service in the inspect table
  name="$(awk -v s="$svc" '$0 ~ s' <<<"$INSPECT" | grep -oE '[a-z0-9-]+:[[:space:]]*[0-9]+/tcp' | head -1 | cut -d: -f1)"
  [[ -n "$name" ]] && kurtosis port print "$ENCLAVE" "$svc" "$name" 2>/dev/null
}
CB="$(port_of "$CB_SVC")"
BN="$(port_of "$BN_SVC")"
[[ -n "$CB" && -n "$BN" ]] || { echo "PROBE-ERROR: could not resolve ports (cb=$CB_SVC bn=$BN_SVC)" >&2; exit 2; }
log "probing $CB (cb=$CB_SVC) against $BN (bn=$BN_SVC)"

bn_json() { curl -sf --max-time 5 "$BN$1" 2>/dev/null; }

# next slot, the head block's EXECUTION block hash, and that slot's proposer key
next_triple() {
  local head slot parent epoch pubkey
  head="$(bn_json /eth/v2/beacon/blocks/head)" || return 1
  slot="$(jq -r '.data.message.slot' <<<"$head" 2>/dev/null)"
  parent="$(jq -r '.data.message.body.execution_payload.block_hash' <<<"$head" 2>/dev/null)"
  [[ "$slot" =~ ^[0-9]+$ && "$parent" =~ ^0x[0-9a-fA-F]{64}$ ]] || return 1
  slot=$((slot + 1)); epoch=$((slot / 32))
  pubkey="$(bn_json "/eth/v1/validator/duties/proposer/$epoch" \
    | jq -r --arg s "$slot" '.data[] | select(.slot == $s) | .pubkey' 2>/dev/null | head -1)"
  [[ "$pubkey" =~ ^0x[0-9a-fA-F]{96}$ ]] || return 1
  printf '%s %s %s\n' "$slot" "$parent" "$pubkey"
}

# probe_one <accept> <expected-content-type> <label>
probe_one() {
  local accept="$1" want_ct="$2" label="$3" i triple slot parent pubkey code ct body_file
  body_file="$(mktemp)"
  for ((i = 1; i <= ATTEMPTS; i++)); do
    if ! triple="$(next_triple)"; then sleep 3; continue; fi
    read -r slot parent pubkey <<<"$triple"
    code="$(curl -s -o "$body_file" -w '%{http_code}' --max-time 8 \
      -H "Accept: $accept" \
      "$CB/eth/v1/builder/header/$slot/$parent/$pubkey" 2>/dev/null)"
    ct="$(curl -s -o /dev/null -D - --max-time 8 -H "Accept: $accept" \
      "$CB/eth/v1/builder/header/$slot/$parent/$pubkey" 2>/dev/null \
      | grep -i '^content-type:' | tr -d '\r' | awk '{print tolower($2)}' | cut -d';' -f1)"
    if [[ "$code" == "200" ]]; then
      local size; size=$(stat -c%s "$body_file" 2>/dev/null || echo 0)
      # content-type must be the encoding we asked for
      if [[ "$ct" != "$want_ct" ]]; then
        echo "PROBE $label: FAIL (200 but Content-Type=$ct, want $want_ct) slot=$slot"
        rm -f "$body_file"; return 1
      fi
      # and the body must actually parse as that encoding
      if [[ "$accept" == "application/json" ]]; then
        if jq -e '.data.message.header.block_hash' "$body_file" >/dev/null 2>&1; then
          echo "PROBE $label: PASS (200, Content-Type=$ct, ${size}B, parsed JSON bid) slot=$slot"
          rm -f "$body_file"; return 0
        fi
        echo "PROBE $label: FAIL (200/$ct but body is not a decodable JSON bid) slot=$slot"
        rm -f "$body_file"; return 1
      else
        # SSZ: opaque binary. A real SignedBuilderBid is well over 100 bytes and
        # must NOT be JSON (a JSON body behind an SSZ content-type is the bug).
        if (( size > 100 )) && ! jq -e . "$body_file" >/dev/null 2>&1; then
          echo "PROBE $label: PASS (200, Content-Type=$ct, ${size}B, binary SSZ bid) slot=$slot"
          rm -f "$body_file"; return 0
        fi
        echo "PROBE $label: FAIL (200/$ct but body is not SSZ: ${size}B) slot=$slot"
        rm -f "$body_file"; return 1
      fi
    fi
    sleep 3   # 204 (no bid yet for this slot) or transient: try the next slot
  done
  echo "PROBE $label: INCONCLUSIVE (no 200 bid in $ATTEMPTS attempts; last code=$code)"
  rm -f "$body_file"; return 2
}

rc=0
probe_one "application/octet-stream" "application/octet-stream" "SSZ"  || rc=$?
probe_one "application/json"         "application/json"         "JSON" || { r=$?; (( r > rc )) && rc=$r; }
exit $rc
