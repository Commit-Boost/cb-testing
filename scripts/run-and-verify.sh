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
    echo "  --timeout SECS      Readiness timeout (default: 1500)"
    echo "  --min-epochs N      Observation window in epochs (default: 2)"
    echo "  --target-epoch N    Wait until this epoch before checks (default: 5)"
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

# Step 2: Launch the devnet
echo "Launching devnet..."
echo "  Package: $PACKAGE"
echo "  Config:  $CONFIG"
echo "  Enclave: $ENCLAVE"
echo ""

kurtosis run "$PACKAGE" \
    --enclave "$ENCLAVE" \
    --args-file "$CONFIG" \
    --image-download always

echo ""
echo "Enclave '$ENCLAVE' is up. Starting verification..."
echo ""

# Step 3: Run verification
cargo run --manifest-path "$REPO_DIR/Cargo.toml" --release -- \
    --enclave "$ENCLAVE" \
    --cb-config "$CONFIG" \
    --timeout "$TIMEOUT" \
    --min-epochs "$MIN_EPOCHS" \
    --target-epoch "$TARGET_EPOCH" \
    $JSON_FLAG \
    $JSON_DIR_FLAG \
    $STRICT_FLAG \
    $LIVE_METRICS_FLAG \
    $VERBOSE
