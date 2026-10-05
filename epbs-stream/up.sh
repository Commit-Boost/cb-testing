#!/usr/bin/env bash
# Bring up the harness: a gloas devnet (mainnet preset, 12 s slots) + a Helix relay with
# a stub simulator + Commit-Boost (get_header = "stream" on the Helix entries) + the
# synthetic gloas bid injector. Leaves the enclave running; down.sh removes it.
#
#   up.sh [all|launch|services|inject|relay]
#
#   launch    kurtosis run ethereum-package with the rendered devnet args
#   services  stub simulator, helix-relay, commit-boost, then cb-km apply
#   inject    gloas_inject (INJECT_ARGS overrides its schedule)
#   relay     replace helix-relay with $HELIX_IMAGE, keeping its config (e.g. to A/B a fix)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

EP_PACKAGE="${EP_PACKAGE:-github.com/ethpandaops/ethereum-package@6dd3f2613d1f9d1a9274864083c692726146c9db}"
DEVNET_ARGS="${DEVNET_ARGS:-$REPO_ROOT/configs/epbs/gloas-epbs.yaml}"
# Helix builds its ChainSpec with lighthouse's from_config, which rejects a spec whose
# SECONDS_PER_SLOT and SLOT_DURATION_MS disagree. lodestar:km reports SECONDS_PER_SLOT=12
# whatever the genesis says, so 6 s slots make helix panic at startup; 12 s is consistent.
SECONDS_PER_SLOT="${SECONDS_PER_SLOT:-12}"
STREAM_FOR_MS="${STREAM_FOR_MS:-300}"
INJECT_ARGS="${INJECT_ARGS:---offsets-ms -1500,-500,100,300,500,1000,1100,1200 --duration-s 7200}"
CB_KM_BIN="${CB_KM_BIN:-$(command -v cb-km || true)}"
KEYS="$HARNESS/keys"
stage="${1:-all}"

case $stage in all|launch|services|inject|relay) ;; *) die "unknown stage '$stage'" ;; esac

if [[ $stage == all || $stage == launch ]]; then
  log "render devnet args ($DEVNET_ARGS: preset mainnet, ${SECONDS_PER_SLOT}s slots)"
  sed -E -e "s/^([[:space:]]*preset:[[:space:]]*).*/\1mainnet/" \
         -e "s/^([[:space:]]*seconds_per_slot:[[:space:]]*).*/\1$SECONDS_PER_SLOT/" \
    "$DEVNET_ARGS" > "$RUN_DIR/args.yaml"
  grep -qE "^[[:space:]]*preset:[[:space:]]*mainnet$" "$RUN_DIR/args.yaml" || die "no preset: line in $DEVNET_ARGS"
  grep -qE "^[[:space:]]*seconds_per_slot:[[:space:]]*$SECONDS_PER_SLOT$" "$RUN_DIR/args.yaml" \
    || die "no seconds_per_slot: line in $DEVNET_ARGS"
  kurtosis enclave inspect "$ENCLAVE" >/dev/null 2>&1 && die "enclave $ENCLAVE exists; run down.sh first"
  log "kurtosis run $EP_PACKAGE"
  kurtosis run "$EP_PACKAGE" --enclave "$ENCLAVE" --args-file "$RUN_DIR/args.yaml" --image-download missing
fi

BN_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE 'cl-1-[a-z0-9-]+' | head -1)"
VC_SVC="$(kurtosis enclave inspect "$ENCLAVE" 2>/dev/null | grep -oE 'vc-1-[a-z0-9-]+' | head -1)"
[[ -n "$BN_SVC" && -n "$VC_SVC" ]] || die "no cl-1-* / vc-1-* service in enclave $ENCLAVE"
BN="$(port_url "$BN_SVC" http)"
VC_KM="$(port_url "$VC_SVC" http-validator)"
BN_INNER_PORT="$(kurtosis service inspect "$ENCLAVE" "$BN_SVC" | grep -E '^\s*http:' | grep -oE '[0-9]+/tcp' | head -1 | cut -d/ -f1)"
BN_INNER="http://$BN_SVC:$BN_INNER_PORT"
echo "BN=$BN ($BN_INNER) VC_KM=$VC_KM"
echo "$BN" > "$RUN_DIR/bn.url"

add_relay() {
  [[ -n "$HELIX_IMAGE" ]] || die "HELIX_IMAGE unset; run build-images.sh <helix-ref> or export it"
  kurtosis service add "$ENCLAVE" helix-relay "$HELIX_IMAGE" --files "/cfg:helix-cfg" \
    --cmd "--config /cfg/config.yml" \
    --env "RELAY_KEY=$(cat "$KEYS/relay.sk"),ADMIN_TOKEN=epbs-stream,POSTGRES_PASSWORD=unused,RUST_LOG=info" \
    --ports "api=http:4040,metrics=http:9500" >/dev/null
  echo "helix-relay: $HELIX_IMAGE"
}

if [[ $stage == all || $stage == services ]]; then
  [[ -x "$CB_KM_BIN" ]] || die "cb-km not found: set CB_KM_BIN (cargo build -p cb-km-tool --release in the CB checkout)"
  docker image inspect "$CB_IMAGE" "$STUB_IMAGE" >/dev/null || die "missing image (CB_IMAGE=$CB_IMAGE STUB_IMAGE=$STUB_IMAGE)"

  log "wait for head >= 1"
  for _ in $(seq 1 60); do
    h=$(curl -sf "$BN/eth/v1/beacon/headers/head" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['header']['message']['slot'])" 2>/dev/null || echo 0)
    (( h >= 1 )) && break
    sleep 3
  done
  (( h >= 1 )) || die "chain did not advance"
  curl -sf "$BN/eth/v1/beacon/genesis" > "$RUN_DIR/genesis.json"
  curl -sf "$BN/eth/v1/config/spec" > "$RUN_DIR/spec.json"
  read -r GEN_TIME GVR SPS SLOT_MS < <(python3 - "$RUN_DIR" <<'EOF'
import json, sys
g = json.load(open(sys.argv[1] + "/genesis.json"))["data"]
s = json.load(open(sys.argv[1] + "/spec.json"))["data"]
print(g["genesis_time"], g["genesis_validators_root"], s["SECONDS_PER_SLOT"], s.get("SLOT_DURATION_MS", "-"))
EOF
)
  echo "genesis=$GEN_TIME SECONDS_PER_SLOT=$SPS SLOT_DURATION_MS=$SLOT_MS"
  [[ "$SPS" == "$SECONDS_PER_SLOT" && "$SLOT_MS" == "$((SECONDS_PER_SLOT * 1000))" ]] \
    || die "BN spec inconsistent with ${SECONDS_PER_SLOT}s slots; helix would panic in ChainSpec::from_config"

  log "render configs"
  mkdir -p "$RUN_DIR/cb" "$RUN_DIR/helix"
  sed -e "s|__SLOT_SECS__|$SECONDS_PER_SLOT|" -e "s|__GENESIS_TIME__|$GEN_TIME|" \
      -e "s|__GENESIS_VALIDATORS_ROOT__|$GVR|" -e "s|__HELIX_URL__|http://$(cat "$KEYS/relay.pk")@helix-relay:4040|g" \
    "$HARNESS/config/cb-config.toml.tmpl" > "$RUN_DIR/cb/cb-config.toml"
  sed -e "s|__BN_URL__|$BN_INNER|" -e "s|__BUILDER_PK__|$(cat "$KEYS/builder.pk")|" \
      -e "s|__ROUTES__|[{route: BuilderApi}, {route: ProposerApi}, {route: DataApi}]|" \
      -e "s|__STREAM_FOR_MS__|$STREAM_FOR_MS|" \
    "$HARNESS/config/helix-config.tmpl.yml" > "$RUN_DIR/helix/config.yml"
  cp "$REPO_ROOT/configs/epbs/km-token.txt" "$RUN_DIR/km-token.txt"
  sed -e "s|__VC_KM_URL__|$VC_KM|" -e "s|__TOKEN_PATH__|$RUN_DIR/km-token.txt|" \
    "$REPO_ROOT/configs/epbs/km-overlay.toml.tmpl" > "$RUN_DIR/km-overlay.toml"
  ! grep -l '__[A-Z_]*__' "$RUN_DIR/cb/cb-config.toml" "$RUN_DIR/helix/config.yml" "$RUN_DIR/km-overlay.toml" \
    || die "unrendered placeholder"

  log "stub simulator"
  kurtosis service add "$ENCLAVE" helix-simstub "$STUB_IMAGE" --ports "rpc=http:8545" >/dev/null

  log "helix relay"
  kurtosis files upload "$ENCLAVE" "$RUN_DIR/helix" --name helix-cfg >/dev/null
  add_relay
  sleep 5
  kurtosis service logs "$ENCLAVE" helix-relay 2>&1 | strip_ansi | tail -5 | cut -c1-200

  log "commit-boost"
  kurtosis files upload "$ENCLAVE" "$RUN_DIR/cb" --name cb-cfg >/dev/null
  kurtosis service add "$ENCLAVE" cb-epbs "$CB_IMAGE" --files "/cb:cb-cfg" \
    --env "CB_CONFIG=/cb/cb-config.toml,CB_METRICS_PORT=10000" \
    --ports "pbs=http:18550,metrics=http:10000" --cmd pbs >/dev/null
  sleep 4
  kurtosis service logs "$ENCLAVE" cb-epbs 2>&1 | strip_ansi | tail -5 | cut -c1-200

  log "cb-km apply (VC builder_config -> cb-epbs)"
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml" > "$RUN_DIR/cb-km-apply.log" 2>&1
  echo "cb-km: $(grep -c '^accepted' "$RUN_DIR/cb-km-apply.log") keys accepted"
fi

if [[ $stage == relay ]]; then
  log "replace helix-relay"
  kurtosis service rm "$ENCLAVE" helix-relay >/dev/null
  add_relay
fi

if [[ $stage == all || $stage == inject ]]; then
  [[ -n "$INJECT_IMAGE" ]] || die "INJECT_IMAGE unset; run build-images.sh <helix-ref> or export it"
  log "injector ($INJECT_IMAGE)"
  kurtosis service add "$ENCLAVE" helix-inject "$INJECT_IMAGE" \
    --cmd "--beacon $BN_INNER --relay http://helix-relay:4040 --builder-sk $(cat "$KEYS/builder.sk") $INJECT_ARGS" >/dev/null
  sleep 3
  kurtosis service logs "$ENCLAVE" helix-inject 2>&1 | tail -3 | cut -c1-200
fi
log "done ($stage)"
