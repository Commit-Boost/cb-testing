#!/usr/bin/env bash
# The stream-on scenario, run against an enclave brought up by up.sh:
#   BID_SLOTS slots with bids -> NOBID_SLOTS slots with the injector stopped -> RESUME_SLOTS+1
#   slots with bids again,
# with a packet capture of cb-epbs <-> helix-relay:4040 throughout. Writes the logs, CB metric
# snapshots at each boundary, the capture and negative.sh's output to OUT, then scores them
# with analyze.py. Takes about (BID_SLOTS + NOBID_SLOTS + RESUME_SLOTS + 4) slots.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

BID_SLOTS="${BID_SLOTS:-6}"
NOBID_SLOTS="${NOBID_SLOTS:-12}"
RESUME_SLOTS="${RESUME_SLOTS:-4}"
# Boundaries fall this far into a slot: after that slot's bid request is answered
# (~1.2 s) and before the injector's first bid for the next slot (1.5 s before it starts).
EDGE_MS="${EDGE_MS:-3000}"
OUT="${OUT:-$RUN_DIR/scenario-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$OUT"
CB_METRICS="$(port_url cb-epbs metrics)/metrics"

snapshot() {
  curl -sf "$CB_METRICS" > "$OUT/cb-metrics-$1.txt"
  echo "snapshot $1 at slot $(current_slot) $(date -u +%T)"
}

log "packet capture ($CAPTURE_NAME in cb-epbs's network namespace)"
docker rm -f "$CAPTURE_NAME" >/dev/null 2>&1 || true
docker run -d --name "$CAPTURE_NAME" --network "container:$(service_container cb-epbs)" "$NETSHOOT_IMAGE" \
  tcpdump -i any -U -w /cap.pcap 'tcp port 4040' >/dev/null

kurtosis service start "$ENCLAVE" helix-inject >/dev/null 2>&1 || true  # no-op when running
S0=$(( $(current_slot) + 1 ))
log "bids from slot $((S0 + 1)) for $BID_SLOTS slots -> $OUT"
wait_slot_ms "$S0" "$EDGE_MS"; snapshot start
wait_slot_ms $((S0 + BID_SLOTS)) "$EDGE_MS"
# gloas_inject runs as PID 1, which ignores SIGTERM, so the stop returns only after Docker's
# 10 s timeout, by when the injector has sent its bids for the next slot. The bid stretch
# therefore ends at the slot in which the stop returns.
kurtosis service stop "$ENCLAVE" helix-inject >/dev/null
PAUSE=$(current_slot)
echo "injector stopped in slot $PAUSE"
wait_slot_ms "$PAUSE" "$EDGE_MS"; snapshot pause
RESUME=$((PAUSE + NOBID_SLOTS))
END=$((RESUME + RESUME_SLOTS + 1))
printf 'S0=%s\nPAUSE=%s\nRESUME=%s\nEND=%s\n' "$S0" "$PAUSE" "$RESUME" "$END" > "$OUT/slots.env"
log "bids $((S0 + 1))..$PAUSE, no bids $((PAUSE + 1))..$RESUME, bids $((RESUME + 1))..$END"
wait_slot_ms "$RESUME" "$EDGE_MS"; snapshot resume
kurtosis service start "$ENCLAVE" helix-inject >/dev/null; echo "injector started"
wait_slot_ms "$END" "$EDGE_MS"; snapshot end

log "negative.sh"
bash "$HARNESS/negative.sh" > "$OUT/negative.out" 2>&1 || true
tail -1 "$OUT/negative.out"

log "collect"
docker stop -t 2 "$CAPTURE_NAME" >/dev/null
docker cp -q "$CAPTURE_NAME:/cap.pcap" "$OUT/cb-helix.pcap"
docker rm "$CAPTURE_NAME" >/dev/null
for s in cb-epbs helix-relay helix-inject helix-simstub; do
  kurtosis service logs -a "$ENCLAVE" "$s" 2>&1 | strip_ansi > "$OUT/$s.log"
done
curl -sf "$(port_url helix-relay metrics)/metrics" > "$OUT/helix-metrics.txt" || true
docker image inspect "$HELIX_IMAGE" "$INJECT_IMAGE" "$CB_IMAGE" | python3 -c '
import json, sys
for i in json.load(sys.stdin):
    print(i["RepoTags"][0], i["Id"], json.dumps((i.get("Config") or {}).get("Labels") or {}))' > "$OUT/images.txt"
# one row per packet; websocket frames are dissected after the HTTP upgrade on :4040
docker run --rm -v "$OUT:/data:ro" "$NETSHOOT_IMAGE" tshark -r /data/cb-helix.pcap -d tcp.port==4040,http \
  -T fields -E separator=/t -E occurrence=a -e frame.time_epoch -e tcp.stream -e tcp.srcport \
  -e tcp.flags.fin -e tcp.flags.reset -e websocket.opcode -e http.request.uri -e http.response.code \
  > "$OUT/cb-helix.tsv"

log "analyze"
python3 "$HARNESS/analyze.py" "$OUT" | tee "$OUT/summary.txt"
