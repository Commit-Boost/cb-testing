#!/usr/bin/env bash
# Run the ePBS (gloas) builder-built sim across a MATRIX of consensus clients and
# print one PASS/FAIL/SKIP row per client. Each client is a scenario file under
# configs/epbs/ (a client needs its own file because clients differ in image count
# and flags - lodestar is one image, prysm is a beacon-chain image + a validator
# image + --enable-builder - which a single CL_IMAGE override cannot express).
#
# Usage:
#   scripts/run-epbs-matrix.sh [config.yaml ...]      # default = every gloas-epbs*.yaml
#   PRESET=mainnet scripts/run-epbs-matrix.sh ...      # forwarded to each run
#   ASSERT=p2p scripts/run-epbs-matrix.sh ...          # --assert mode for each run
#
# A client whose image is a local/* build that is not present is SKIPPED (not
# failed), so a fresh clone still runs the clients it has (e.g. lodestar from Docker
# Hub) and only skips the ones that need a local build (e.g. prysm). Public images
# are assumed pullable. Runs are SEQUENTIAL - the devnet stack is heavy and the box
# is shared. Exit is non-zero iff at least one client FAILED (SKIP does not fail).
set -u -o pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PRESET="${PRESET:-}"
ASSERT="${ASSERT:-}"
SP="${SP:-configs/epbs/.cb-matrix}"   # per-client run logs land here (gitignored)

# default matrix = every SINGLE-CLIENT CB-in-loop scenario under configs/epbs/. A
# scenario qualifies by content, not name: exactly one `cl_type:` (so the multi-CL
# assertoor configs gloas-epbs-2cl.yaml/gloas-epbs-matrix.yaml are excluded) and
# `mev_type: buildoor` (the CB-in-loop shape). The *-mainnet.yaml variants are the
# same client at a heavier preset and are opt-in via an explicit arg. An explicit
# arg list bypasses this filter (you named the files, we run them).
if [[ $# -gt 0 ]]; then
  CONFIGS=("$@")
else
  CONFIGS=()
  for f in configs/epbs/gloas-epbs-*.yaml; do
    [[ "$f" == *-mainnet.yaml ]] && continue
    [[ "$(grep -cE '^[[:space:]]*-?[[:space:]]*cl_type:' "$f")" == "1" ]] || continue
    grep -qE '^[[:space:]]*mev_type:[[:space:]]*buildoor' "$f" || continue
    CONFIGS+=("$f")
  done
fi
(( ${#CONFIGS[@]} > 0 )) || { echo "no scenario files to run" >&2; exit 2; }

# yaml_field <file> <key> -> first "  key: value" value (strips inline comment/quotes)
yaml_field() {
  sed -nE "s/^[[:space:]]*$2:[[:space:]]*([^#[:space:]]+).*/\1/p" "$1" | head -1 | tr -d '"'
}

# image_ok <image> -> 0 if present locally OR looks pullable (non local/*). A local/*
# image must exist locally; anything else we assume a registry has.
image_ok() {
  case "$1" in
    local/*) docker image inspect "$1" >/dev/null 2>&1 ;;
    *)       return 0 ;;
  esac
}

mkdir -p "$SP"
declare -a ROWS
overall_fail=0

for cfg in "${CONFIGS[@]}"; do
  [[ -f "$cfg" ]] || { ROWS+=("$(printf '%-24s %-9s %-6s %s' "$(basename "$cfg")" "-" "SKIP" "no such file")"); continue; }
  cl="$(yaml_field "$cfg" cl_type)"; cl="${cl:-?}"
  label="$(basename "$cfg" .yaml)"
  cl_image="$(yaml_field "$cfg" cl_image)"
  vc_image="$(yaml_field "$cfg" vc_image)"; vc_image="${vc_image:-$cl_image}"
  preset="${PRESET:-$(yaml_field "$cfg" preset)}"; preset="${preset:-minimal}"

  # preflight: every configured image must be runnable, else SKIP with the reason
  missing=""
  for img in "$cl_image" "$vc_image"; do
    [[ -n "$img" ]] && ! image_ok "$img" && missing="$img"
  done
  if [[ -n "$missing" ]]; then
    echo ">>> SKIP $label ($cl): image not present locally: $missing"
    ROWS+=("$(printf '%-24s %-9s %-6s %s' "$label" "$preset" "SKIP" "missing $missing")")
    continue
  fi

  echo ">>> RUN  $label  cl=$cl  preset=$preset  assert=${ASSERT:-default(builder-built)}"
  logf="$SP/$label.log"
  assert_args=(); [[ -n "$ASSERT" ]] && assert_args=(--assert "$ASSERT")
  ENCLAVE="epbs-mtx-$label" ARGS_FILE="$cfg" PRESET="$preset" \
    ./scripts/run-epbs-sim.sh "${assert_args[@]}" >"$logf" 2>&1
  rc=$?

  # pull the builder-built ratio from the PASS/summary line if present
  ratio="$(grep -oE '[0-9]+/[0-9]+ observed slots builder-built' "$logf" | tail -1 | grep -oE '^[0-9]+/[0-9]+')"
  ratio="${ratio:-n/a}"
  if (( rc == 0 )); then
    ROWS+=("$(printf '%-24s %-9s %-6s %s' "$label" "$preset" "PASS" "$ratio")")
  else
    overall_fail=1
    reason="$(grep -iE '^(die:|ERROR|FATAL|block-submission failed|.*failed)' "$logf" | tail -1 | cut -c1-60)"
    ROWS+=("$(printf '%-24s %-9s %-6s %s' "$label" "$preset" "FAIL" "rc=$rc ${reason:-see $logf}")")
  fi
done

echo
echo "================ ePBS CL matrix ================"
printf '%-24s %-9s %-6s %s\n' "CLIENT" "PRESET" "RESULT" "DETAIL"
printf '%s\n' "${ROWS[@]}"
echo "================================================"
echo "logs: $SP/<client>.log"
exit $overall_fail
