#!/usr/bin/env bash
# Hand-made websocket upgrades to Helix's ePBS bid stream with a bad X-Request-Auth.
# Every case must be answered with a 4xx before any 101 Switching Protocols.
# Exits non-zero unless all five are refused.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
[[ -n "$INJECT_IMAGE" ]] || die "INJECT_IMAGE unset; run build-images.sh <helix-ref> or export it"

BN="$(bn_url)"
HELIX="$(port_url helix-relay api)"
SK="$(cat "$HARNESS/keys/builder.sk")"
# gloas_inject signs the request auths; it talks to the host-mapped ports
inject() { docker run --rm --network host "$INJECT_IMAGE" --beacon "$BN" --relay "$HELIX" --builder-sk "$SK" "$@"; }

head=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])")
N=$((head + 1))
PK=$(curl -sf "$HELIX/relay/v1/builder/validators" | python3 -c "
import sys,json
for e in json.load(sys.stdin):
    if int(e['slot'])==$N: print(e['entry']['message']['pubkey'])")
[[ -n "$PK" ]] || die "no proposer duty at helix for slot $N"
Z=0x$(printf '0%.0s' {1..64})
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
probe() { # $1=label $2=auth header value (empty to omit)
  local hdr=() out status
  [[ -n "$2" ]] && hdr=(-H "X-Request-Auth: $2")
  echo "--- $1"
  out=$(curl -s -i --max-time 3 --http1.1 \
    -H "Connection: Upgrade" -H "Upgrade: websocket" -H "Sec-WebSocket-Version: 13" \
    -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" -H "Eth-Consensus-Version: gloas" \
    -H "X-Timeout-Ms: 1000" -H "Date-Milliseconds: $(date +%s%3N)" "${hdr[@]}" \
    "$HELIX/eth/v1/builder/execution_payload_bid_stream/$N/$Z/$Z/$PK" | tr -d '\r')
  echo "$out" | sed -n '1p;/^[Uu]pgrade:/p;/^$/,$p' | head -4
  echo
  status=$(echo "$out" | head -1 | awk '{print $2}')
  ! grep -q " 101 " <<< "$out" && [[ $status == 4?? ]] && refused=$((refused + 1))
}
probe "A: well-formed auth for the slot, validly signed by a NON-proposer key" "$WRONG_KEY"
probe "B: same auth with 10 signature bytes flipped (corrupted)" "$CORRUPT"
probe "C: auth signed for slot N+7 (slot mismatch)" "$WRONG_SLOT"
probe "D: X-Request-Auth not base64" "not*base64"
probe "E: X-Request-Auth missing" ""

if (( refused == 5 )); then echo "PASS: 5/5 refused with 4xx, no 101"; else echo "FAIL: $refused/5 refused with 4xx"; exit 1; fi
