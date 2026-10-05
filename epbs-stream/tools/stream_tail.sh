#!/usr/bin/env bash
# Downlink-only latency sweep for Helix's ePBS bid stream, run against an enclave brought
# up by cb-testing's epbs-stream/up.sh. Adds tc netem delay to helix-relay's egress toward
# cb-epbs only (prio qdisc + u32 dst filter + netem on band 3), steps it through DELAYS,
# holds each for SLOTS_PER slots, verifies each step with ping and a one-way UDP probe in
# both directions, captures cb-epbs <-> helix-relay:4040 in CB's namespace throughout, and
# collects logs + metrics into OUT.
set -euo pipefail
TOOLS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HARNESS="${HARNESS:-$(cd "$TOOLS/.." && pwd)}"
source "$HARNESS/lib.sh"

DELAYS="${DELAYS:-0 100 200 300 400}"
SLOTS_PER="${SLOTS_PER:-8}"
CONTROL_SLOTS="${CONTROL_SLOTS:-4}"   # a final D=0 stretch, to show the effect reverses
EDGE_MS="${EDGE_MS:-3000}"
NETEM="$ENCLAVE-netem"
OUT="${OUT:-$RUN_DIR/stream-tail-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$OUT"

HELIX_C="$(service_container helix-relay)"
CB_C="$(service_container cb-epbs)"
STUB_C="$(service_container helix-simstub)"
ip_of() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' "$1" | awk '{print $1}'; }
HELIX_IP="$(ip_of "$HELIX_C")"
CB_IP="$(ip_of "$CB_C")"
STUB_IP="$(ip_of "$STUB_C")"
echo "helix=$HELIX_IP cb=$CB_IP stub=$STUB_IP" | tee "$OUT/ips.txt"
CB_METRICS="$(port_url cb-epbs metrics)/metrics"

log "capture sidecar ($CAPTURE_NAME in cb-epbs's namespace) + netem sidecar ($NETEM in helix-relay's)"
docker rm -f "$CAPTURE_NAME" "$NETEM" >/dev/null 2>&1 || true
docker run -d --name "$CAPTURE_NAME" --network "container:$CB_C" "$NETSHOOT_IMAGE" \
  tcpdump -i any -U -w /cap.pcap 'tcp port 4040' >/dev/null
docker run -d --name "$NETEM" --cap-add NET_ADMIN --network "container:$HELIX_C" "$NETSHOOT_IMAGE" sleep infinity >/dev/null
docker cp -q "$TOOLS/owd.py" "$CAPTURE_NAME:/owd.py"
docker cp -q "$TOOLS/owd.py" "$NETEM:/owd.py"
docker exec "$NETEM" ip -br addr | tee "$OUT/helix-ifaces.txt"
IFACE="$(docker exec "$NETEM" ip -o -4 addr show | awk -v ip="$HELIX_IP" '$4 ~ "^"ip"/" {print $2}')"
[[ -n "$IFACE" ]] || die "no interface holds $HELIX_IP"
# every packet defaults to band 2 (1:2); only packets to CB's IP go to band 3 (1:3), where netem sits
docker exec "$NETEM" tc qdisc add dev "$IFACE" root handle 1: prio bands 3 priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1
docker exec "$NETEM" tc qdisc add dev "$IFACE" parent 1:3 handle 30: netem delay 0ms
docker exec "$NETEM" tc filter add dev "$IFACE" parent 1: protocol ip prio 1 u32 match ip dst "$CB_IP/32" flowid 1:3

verify() {
  local d=$1 f="$OUT/verify-$1ms.txt"
  set +e
  {
    echo "## D=${d}ms  $(date -u +%T.%N)"
    docker exec "$NETEM" tc -s qdisc show dev "$IFACE"
    docker exec "$NETEM" tc filter show dev "$IFACE"
    echo "-- ping cb -> helix (RTT; the echo reply rides the delayed band)"
    docker exec "$CAPTURE_NAME" ping -c 5 -i 0.2 -q "$HELIX_IP" | tail -1
    echo "-- ping helix-simstub -> helix (control: not CB, not delayed)"
    docker run --rm --network "container:$STUB_C" "$NETSHOOT_IMAGE" ping -c 3 -i 0.2 -q "$HELIX_IP" | tail -1
    echo "-- one-way cb -> helix (uplink, must stay ~0)"
    docker exec "$NETEM" python3 /owd.py recv 9999 10 4 > "$OUT/.owd-up" &
    sleep 0.3; docker exec "$CAPTURE_NAME" python3 /owd.py send "$HELIX_IP" 9999 10; wait; cat "$OUT/.owd-up"
    echo "-- one-way helix -> cb (downlink, must be ~D)"
    docker exec "$CAPTURE_NAME" python3 /owd.py recv 9998 10 4 > "$OUT/.owd-down" &
    sleep 0.3; docker exec "$NETEM" python3 /owd.py send "$CB_IP" 9998 10; wait; cat "$OUT/.owd-down"
  } > "$f" 2>&1
  rm -f "$OUT/.owd-up" "$OUT/.owd-down"
  grep -E "^(rtt|n=)" "$f" | tr '\n' ' '; echo
  set -e
}

set_delay() { docker exec "$NETEM" tc qdisc change dev "$IFACE" parent 1:3 handle 30: netem delay "${1}ms"; }

kurtosis service start "$ENCLAVE" helix-inject >/dev/null 2>&1 || true
S=$(( $(current_slot) + 1 ))
: > "$OUT/delays.tsv"
step() {  # step <delay_ms> <slots>
  local d=$1 n=$2
  wait_slot_ms "$S" "$EDGE_MS"
  set_delay "$d"
  log "D=${d}ms from slot $((S + 1)) to $((S + n))"
  verify "$d"
  curl -sf "$CB_METRICS" > "$OUT/cb-metrics-before-${d}ms-s$((S + 1)).txt" || true
  printf '%s\t%s\t%s\n' "$d" "$((S + 1))" "$((S + n))" >> "$OUT/delays.tsv"
  S=$((S + n))
}
for d in $DELAYS; do step "$d" "$SLOTS_PER"; done
if (( CONTROL_SLOTS > 0 )); then step 0 "$CONTROL_SLOTS"; fi
wait_slot_ms "$S" "$EDGE_MS"
curl -sf "$CB_METRICS" > "$OUT/cb-metrics-end.txt" || true

log "collect"
docker exec "$NETEM" tc -s qdisc show dev "$IFACE" > "$OUT/tc-final.txt" 2>&1 || true
docker exec "$NETEM" tc qdisc del dev "$IFACE" root
docker rm -f "$NETEM" >/dev/null && echo "removed $NETEM"
docker stop -t 2 "$CAPTURE_NAME" >/dev/null
docker cp -q "$CAPTURE_NAME:/cap.pcap" "$OUT/cb-helix.pcap"
docker rm "$CAPTURE_NAME" >/dev/null
for s in cb-epbs helix-relay helix-inject helix-simstub; do
  kurtosis service logs -a "$ENCLAVE" "$s" 2>&1 | strip_ansi > "$OUT/$s.log"
done
curl -sf "$(port_url helix-relay metrics)/metrics" > "$OUT/helix-metrics.txt" || true
cp "$RUN_DIR/genesis.json" "$RUN_DIR/spec.json" "$RUN_DIR/cb/cb-config.toml" "$RUN_DIR/helix/config.yml" "$OUT/"
docker image inspect "$HELIX_IMAGE" "$INJECT_IMAGE" "$CB_IMAGE" | python3 -c '
import json, sys
for i in json.load(sys.stdin):
    print(i["RepoTags"][0], i["Id"], json.dumps((i.get("Config") or {}).get("Labels") or {}))' > "$OUT/images.txt"
docker run --rm -v "$OUT:/data:ro" "$NETSHOOT_IMAGE" tshark -r /data/cb-helix.pcap -d tcp.port==4040,http \
  -T fields -E separator=/t -E occurrence=a -e frame.time_epoch -e tcp.stream -e tcp.srcport \
  -e tcp.flags.fin -e tcp.flags.reset -e websocket.opcode -e http.request.uri -e http.response.code \
  -e tcp.len > "$OUT/cb-helix.tsv"
log "done -> $OUT"
