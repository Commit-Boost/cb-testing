#!/usr/bin/env bash
# Hand-made websocket upgrades to Helix's ePBS bid stream with a bad X-Request-Auth or a
# path pubkey that is not the slot's proposer. Every case must be answered with a 4xx before
# any 101 Switching Protocols, carrying the message of the check meant to refuse it, so an
# earlier check (e.g. "request for wrong slot") cannot pass a case by accident.
# Exits non-zero unless all seven are refused that way.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
[[ -n "$INJECT_IMAGE" ]] || die "INJECT_IMAGE unset; run build-images.sh <helix-ref> or export it"

BN="$(bn_url)"
HELIX="$(port_url helix-relay api)"
SK="$(cat "$HARNESS/keys/builder.sk")"
# gloas_inject signs the request auths; it talks to the host-mapped ports
inject() { docker run --rm --network host "$INJECT_IMAGE" --beacon "$BN" --relay "$HELIX" --builder-sk "$SK" "$@"; }

# Helix moves its head on the BN's head event, about 4 s into the slot. Until then it still
# bids for the current slot and refuses a request for the next one as "wrong slot", before any
# auth check. Probe from 5 s into a slot, so BN head + 1 is the slot Helix is bidding for.
wait_slot_ms $(( $(current_slot) + 1 )) 5000
head=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])")
N=$((head + 1))
PK=$(curl -sf "$HELIX/relay/v1/builder/validators" | python3 -c "
import sys,json
for e in json.load(sys.stdin):
    if int(e['slot'])==$N: print(e['entry']['message']['pubkey'])")
[[ -n "$PK" ]] || die "no proposer duty at helix for slot $N"
Z=0x$(printf '0%.0s' {1..64})
BUILDER_PK="$(cat "$HARNESS/keys/builder.pk")"
# one 4-char quantum past the cap: base64 of the largest valid ssz SignedBuilderRequestAuth
# (112 fixed bytes + 4096 of data) is 5612 chars
OVERSIZED=$(printf 'A%.0s' $(seq 1 5616))
echo "head=$head target_slot=$N proposer=$PK helix=$HELIX"

WRONG_KEY=$(inject --auth-for-slot "$N")
WRONG_SLOT=$(inject --auth-for-slot $((N + 7)))
# flip bytes inside the 96-byte signature (ssz offset 4..100) of the wrong-key auth
CORRUPT=$(python3 -c "
import base64
b=bytearray(base64.b64decode('$WRONG_KEY'))
for i in range(10,20): b[i]^=0xff
print(base64.b64encode(bytes(b)).decode())")

refused=0
probe() { # $1=label $2=auth header value (empty to omit) [$3=path pubkey] [$4=required body text]
  local hdr=() out status pk="${3:-$PK}" want="${4:-}"
  [[ -n "$2" ]] && hdr=(-H "X-Request-Auth: $2")
  echo "--- $1"
  out=$(curl -s -i --max-time 3 --http1.1 \
    -H "Connection: Upgrade" -H "Upgrade: websocket" -H "Sec-WebSocket-Version: 13" \
    -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" -H "Eth-Consensus-Version: gloas" \
    -H "X-Timeout-Ms: 1000" -H "Date-Milliseconds: $(date +%s%3N)" "${hdr[@]}" \
    "$HELIX/eth/v1/builder/execution_payload_bid_stream/$N/$Z/$Z/$pk" | tr -d '\r')
  echo "$out" | sed -n '1p;/^[Uu]pgrade:/p;/^$/,$p' | head -4
  echo
  status=$(echo "$out" | head -1 | awk '{print $2}')
  ! grep -q " 101 " <<< "$out" && [[ $status == 4?? ]] && [[ -z $want || $out == *"$want"* ]] \
    && refused=$((refused + 1))
}
probe "A: well-formed auth for the slot, validly signed by a NON-proposer key" "$WRONG_KEY" "" \
  "signature verification failed"
probe "B: same auth with 10 signature bytes flipped (corrupted)" "$CORRUPT" "" "signature verification failed"
probe "C: auth signed for slot N+7 (slot mismatch)" "$WRONG_SLOT" "" "does not match the request slot"
probe "D: X-Request-Auth not base64" "not*base64" "" "not base64"
probe "E: X-Request-Auth missing" "" "" "header: missing"
# the duty check refuses before any BLS verify: the auth is validly signed by the path's key
probe "F: NON-proposer pubkey in the path, auth validly signed by that key" "$WRONG_KEY" "$BUILDER_PK" \
  "the proposer for the slot is"
probe "G: X-Request-Auth of 5616 base64 chars (cap 5612)" "$OVERSIZED" "" "too long"

if (( refused == 7 )); then echo "PASS: 7/7 refused with 4xx by the intended check, no 101"
else echo "FAIL: $refused/7 refused with 4xx by the intended check"; exit 1; fi
