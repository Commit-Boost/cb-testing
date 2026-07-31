#!/usr/bin/env bash
#
# run-and-verify.sh: Spin up a Kurtosis devnet, verify the MEV pipeline, tear down.
#
# Usage:
#   ./scripts/run-and-verify.sh [--config configs/basic-pbs.yml] [--enclave CB-Testnet]
#                               [--package /path/to/ethereum-package] [--keep] [--json]
#                               [--timeout 1500] [--min-epochs 2]
#
# Exit codes: 0=all checks passed, 1=check failure, 2=setup failure

set -euo pipefail

# Defaults
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
ENCLAVE="CB-Testnet"
CONFIG=""
PACKAGE="$REPO_DIR/ethereum-package"
KEEP=false
JSON_FLAG=""
JSON_DIR_FLAG=""
STRICT_FLAG=""
LIVE_METRICS_FLAG=""
SKIP_FINALIZATION_FLAG=""
TIMEOUT=3600
MIN_EPOCHS=2
TARGET_EPOCH=7
VERBOSE=""

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --config FILE       Kurtosis config file (default: configs/basic-pbs.yml)"
    echo "  --enclave NAME      Enclave name (default: CB-Testnet)"
    echo "  --package PATH      ethereum-package path or ref (default: ./ethereum-package)"
    echo "  --keep              Don't tear down the enclave on exit"
    echo "  --json              Output JSON report"
    echo "  --json-dir DIR      Save JSON report to DIR/{enclave}.json (implies --json)"
    echo "  --strict            Promote WARN to FAIL (zero bids, zero deliveries)"
    echo "  --live-metrics      Show counter deltas every 30s during observation"
    echo "  --skip-finalization Skip chain finality check (use when observing early epochs)"
    echo "  --timeout SECS      Readiness timeout (default: 1500)"
    echo "  --min-epochs N      Observation window in epochs (default: 2)"
    echo "  --target-epoch N    Observation window starts at this epoch (default: 5)"
    echo "  -v, --verbose       Verbose logging"
    echo "  -h, --help          Show this help"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --config)     CONFIG="$2"; shift 2;;
        --enclave)    ENCLAVE="$2"; shift 2;;
        --package)    PACKAGE="$2"; shift 2;;
        --keep)       KEEP=true; shift;;
        --json)       JSON_FLAG="--json"; shift;;
        --json-dir)   JSON_DIR_FLAG="--output-dir $2"; JSON_FLAG="--json"; shift 2;;
        --strict)     STRICT_FLAG="--strict"; shift;;
        --live-metrics) LIVE_METRICS_FLAG="--live-metrics"; shift;;
        --skip-finalization) SKIP_FINALIZATION_FLAG="--skip-finalization-check"; shift;;
        --timeout)    TIMEOUT="$2"; shift 2;;
        --min-epochs) MIN_EPOCHS="$2"; shift 2;;
        --target-epoch) TARGET_EPOCH="$2"; shift 2;;
        -v|--verbose) VERBOSE="-v"; shift;;
        -h|--help)    usage;;
        *)            echo "Unknown option: $1"; exit 2;;
    esac
done

# Default config
if [[ -z "$CONFIG" ]]; then
    CONFIG="$REPO_DIR/configs/basic-pbs.yml"
fi

# Default --json implies auto-save to repo root
if [[ -n "$JSON_FLAG" && -z "$JSON_DIR_FLAG" ]]; then
    JSON_DIR_FLAG="--output-dir $REPO_DIR"
fi

# Resolve config path relative to CWD
if [[ ! -f "$CONFIG" ]]; then
    echo "Config file not found: $CONFIG"
    exit 2
fi

# Cleanup trap
cleanup() {
    if [[ "$KEEP" == "false" ]]; then
        echo ""
        echo "Tearing down enclave '$ENCLAVE'..."
        kurtosis enclave rm -f "$ENCLAVE" 2>/dev/null || true
    else
        echo ""
        echo "Keeping enclave '$ENCLAVE' (--keep flag set)"
        echo "  Inspect: kurtosis enclave inspect $ENCLAVE"
        echo "  Remove:  kurtosis enclave rm -f $ENCLAVE"
    fi
}
trap cleanup EXIT

# Step 1: Clean any stale enclave with the same name
echo "Cleaning stale enclave '$ENCLAVE' (if any)..."
kurtosis enclave rm -f "$ENCLAVE" 2>/dev/null || true

# Step 1b: Preflight gate — validate the config against the real images (~1s) BEFORE the ~10-min run.
# `sim preflight` exits nonzero ONLY on a genuine config-drift Fail (Inconclusive/Pass proceed), so a
# schema drift is caught here as a labeled failure instead of a masked runtime panic minutes into launch.
# Best-effort: if the `sim` bin can't be built/run, warn and proceed (don't block the run on tooling).
# LIMITATION (P1): preflight parses with whatever `:main` image is cached LOCALLY, while the run below pulls
# with `--image-download always` — a `:main` that drifts between the two is a false-green window. Closing it
# needs pull-then-pin-by-digest, which belongs to `sim run` (P2) that owns the pull.
echo "Preflighting config against real images..."
if cargo run --quiet --bin sim --manifest-path "$REPO_DIR/Cargo.toml" -- preflight "$CONFIG"; then
    echo "Preflight OK (no config drift)."
else
    pf_rc=$?
    if [[ $pf_rc -eq 1 ]]; then
        echo "PREFLIGHT FAILED: config drift detected (see the Fail{field} above). Aborting launch." >&2
        exit 1
    fi
    echo "Preflight could not run (rc=$pf_rc); proceeding without the gate." >&2
fi
echo ""

# Step 2: Launch the devnet
echo "Launching devnet..."
echo "  Package: $PACKAGE"
echo "  Config:  $CONFIG"
echo "  Enclave: $ENCLAVE"
echo ""

# On any launch failure, auto-fire triage (root-cause capture as a run property) before exiting.
if ! kurtosis run "$PACKAGE" \
    --enclave "$ENCLAVE" \
    --args-file "$CONFIG" \
    --image-download always; then
    echo ""
    echo "LAUNCH FAILED — triaging crashed services (structured root-cause capture)..." >&2
    cargo run --quiet --bin sim --manifest-path "$REPO_DIR/Cargo.toml" -- triage "$ENCLAVE" || true
    exit 1
fi

echo ""
echo "Enclave '$ENCLAVE' is up. Starting verification..."
echo ""

# Step 3: Run verification
cargo run --bin cb-verify --manifest-path "$REPO_DIR/Cargo.toml" --release -- \
    --enclave "$ENCLAVE" \
    --config "$CONFIG" \
    --timeout "$TIMEOUT" \
    --min-epochs "$MIN_EPOCHS" \
    --target-epoch "$TARGET_EPOCH" \
    $JSON_FLAG \
    $JSON_DIR_FLAG \
    $STRICT_FLAG \
    $LIVE_METRICS_FLAG \
    $SKIP_FINALIZATION_FLAG \
    $VERBOSE
