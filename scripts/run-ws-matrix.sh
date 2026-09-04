#!/usr/bin/env bash
# Exhaustive websocket + encoding matrix for a given commit-boost image.
#
#   CB_IMAGE=ghcr.io/commit-boost/commit-boost:v0.11.0-rc1 scripts/run-ws-matrix.sh
#
# Axes:
#   client pair  x  getHeader transport (ws stream | http baseline)
# plus the ws-stream-nokey NEGATIVE CONTROL, which is EXPECTED to fail the
# feature proof: that failure is what proves the ws criteria discriminate at all
# (the HTTP fallback keeps every MEV check green when the stream is broken, so a
# green MEV run is NOT evidence the stream served).
#
# Per cell:
#   1. compose the scenario config  (sim scenario --base ... --set clients=...)
#   2. run-and-verify               (ws cells add --require-feature-proof)
#   3. probe BOTH wire encodings against the live enclave (scripts/probe-encodings.sh)
#   4. reap the enclave
# Runs are SEQUENTIAL: one devnet at a time (~15G) and the box is shared.
set -u -o pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"; cd "$REPO"

CB_IMAGE="${CB_IMAGE:-ghcr.io/commit-boost/commit-boost:v0.11.0-rc1}"
OUT="${OUT:-configs/generated/matrix}"          # composed configs (gitignored)
RESULTS="${RESULTS:-/tmp/ws-matrix}"            # logs + json reports
CLIENTS="${CLIENTS:-geth-lighthouse nethermind-prysm geth-teku geth-nimbus geth-lodestar}"
TRANSPORTS="${TRANSPORTS:-ws http}"
MIN_EPOCHS="${MIN_EPOCHS:-1}"
TARGET_EPOCH="${TARGET_EPOCH:-1}"
NEG_CONTROL="${NEG_CONTROL:-1}"                 # 1 = also run ws-stream-nokey

mkdir -p "$OUT" "$RESULTS"
log()  { printf '\033[1;36m==> %s\033[0m\n' "$*"; }
row()  { printf '%-22s %-6s %-9s %-11s %-11s %s\n' "$@"; }

# base scenario per transport: ws-stream arms feature.ws_header_stream, cb-basic
# is the HTTP control on the same pipeline.
base_for() { case "$1" in ws) echo cb-ws-stream;; http) echo cb-basic;; *) echo cb-basic;; esac; }

compose() { # <base> <clients> <outfile>
  cargo run --quiet --bin sim -- scenario --base "$1" --set "clients=$2" --out "$3" >/dev/null 2>&1
}

declare -a ROWS
overall=0

# Is the mev builder's own CL following the chain? The builder can only bid on a
# head it has; if its CL is isolated (no peers) or stalled, EVERY downstream MEV
# assertion fails and reads as a commit-boost regression when the real cause is
# the devnet's client interop. Report that distinctly instead of blaming CB.
builder_cl_health() { # <enclave> -> "ok" | "isolated:<peers>/<distance>" | "unknown"
  local enclave="$1" insp bcl peers dist
  insp="$(kurtosis enclave inspect "$enclave" 2>/dev/null)"
  local svc; svc="$(awk '/RUNNING/' <<<"$insp" | grep -oE 'cl-2-[a-z0-9-]+' | head -1)"
  [[ -n "$svc" ]] || { echo unknown; return; }
  bcl="$(kurtosis port print "$enclave" "$svc" http 2>/dev/null)"
  [[ -n "$bcl" ]] || { echo unknown; return; }
  peers="$(curl -sf --max-time 5 "$bcl/eth/v1/node/peer_count" 2>/dev/null | jq -r '.data.connected' 2>/dev/null)"
  dist="$(curl -sf --max-time 5 "$bcl/eth/v1/node/syncing" 2>/dev/null | jq -r '.data.sync_distance' 2>/dev/null)"
  [[ "$peers" =~ ^[0-9]+$ && "$dist" =~ ^[0-9]+$ ]] || { echo unknown; return; }
  if (( peers == 0 )) || (( dist > 8 )); then echo "isolated:${peers}p/${dist}d"; else echo ok; fi
}

run_cell() { # <label> <config> <ws?> <enclave>
  local label="$1" cfg="$2" is_ws="$3" enclave="$4"
  local logf="$RESULTS/$label.log" rc probe_rc ws_proof ssz json health
  local extra=(); [[ "$is_ws" == "1" ]] && extra=(--require-feature-proof)

  log "CELL $label  (config=$cfg${extra:+ , feature-proof})"
  ./scripts/run-and-verify.sh --config "$cfg" --enclave "$enclave" \
      --json --json-dir "$RESULTS" --keep \
      --min-epochs "$MIN_EPOCHS" --target-epoch "$TARGET_EPOCH" \
      "${extra[@]}" >"$logf" 2>&1
  rc=$?

  # Classify the environment BEFORE reading the verdict: an isolated builder CL
  # invalidates every MEV assertion downstream of it.
  health="$(builder_cl_health "$enclave")"
  echo "BUILDER-CL-HEALTH: $health" >>"$logf"

  # encoding probe against the still-running enclave
  ./scripts/probe-encodings.sh "$enclave" >>"$logf" 2>&1
  probe_rc=$?
  ssz="$(grep -oE 'PROBE SSZ: [A-Z]+' "$logf" | tail -1 | awk '{print $3}')"
  json="$(grep -oE 'PROBE JSON: [A-Z]+' "$logf" | tail -1 | awk '{print $3}')"

  kurtosis enclave rm -f "$enclave" >/dev/null 2>&1

  # Did the ws stream actually SERVE, or silently fall back to HTTP? The report is
  # pretty-printed JSON, so read the ws_header_stream check's own result with jq
  # (grep -A on the id is the fallback when jq or the report is unavailable).
  if [[ "$is_ws" == "1" ]]; then
    local res
    res="$(jq -r '.. | objects | select(.id? == "feature.ws_header_stream") | .result' \
            "$RESULTS/$enclave.json" 2>/dev/null | head -1)"
    if [[ -z "$res" ]]; then
      res="$(grep -A3 '"feature.ws_header_stream"' "$logf" 2>/dev/null \
             | grep -oE '"result": "[A-Z]+"' | head -1 | grep -oE '[A-Z]+')"
    fi
    case "$res" in
      PASS) ws_proof=PROVEN ;;
      "")   ws_proof=NO-PROOF ;;
      *)    ws_proof="$res" ;;
    esac
  else ws_proof="n/a"; fi

  # A failure behind an isolated builder CL is an ENVIRONMENT result, not a CB
  # regression: it does not fail the matrix, and it is labelled so nobody reads
  # it as "commit-boost broke".
  local verdict
  if (( rc == 0 )); then verdict=PASS
  elif [[ "$health" == isolated:* ]]; then verdict="ENV(${health#isolated:})"
  else verdict="FAIL($rc)"; overall=1; fi
  ROWS+=("$(row "$label" "$( [[ $is_ws == 1 ]] && echo ws || echo http)" "$verdict" "${ws_proof}" "${ssz:-none}" "${json:-none}")")
}

for cl in $CLIENTS; do
  for tr in $TRANSPORTS; do
    label="$tr-$cl"
    cfg="$OUT/$label.yml"
    if ! compose "$(base_for "$tr")" "$cl" "$cfg"; then
      ROWS+=("$(row "$label" "$tr" "GEN-FAIL" "-" "-" "-")"); overall=1; continue
    fi
    # pin the image under test into the composed config
    sed -i -E "s#^(\s*mev_boost_image:\s*).*#\1$CB_IMAGE#" "$cfg"
    run_cell "$label" "$cfg" "$( [[ $tr == ws ]] && echo 1 || echo 0)" "wsm-$label"
  done
done

# negative control: stream configured but admission disabled -> must NOT prove
if [[ "$NEG_CONTROL" == "1" ]]; then
  cfg="$OUT/negctl-nokey.yml"
  if compose cb-ws-stream-nokey geth-lighthouse "$cfg"; then
    sed -i -E "s#^(\s*mev_boost_image:\s*).*#\1$CB_IMAGE#" "$cfg"
    run_cell "negctl-nokey" "$cfg" 1 "wsm-negctl"
  fi
fi

echo
echo "================== WS / ENCODING MATRIX =================="
echo "image: $CB_IMAGE"
row "CELL" "MODE" "VERIFY" "WS-STREAM" "SSZ" "JSON"
printf '%s\n' "${ROWS[@]}"
echo "=========================================================="
echo "NOTE: negctl-nokey is the negative control - a NO-PROOF there is CORRECT"
echo "      (it proves the ws criteria discriminate). logs: $RESULTS/<cell>.log"
exit $overall
