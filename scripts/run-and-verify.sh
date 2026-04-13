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
ENCLAVE="CB-Testnet"
CONFIG=""
PACKAGE="github.com/ethpandaops/ethereum-package"
KEEP=false
JSON_FLAG=""
TIMEOUT=1500
MIN_EPOCHS=2
VERBOSE=""
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --config FILE       Kurtosis config file (default: configs/basic-pbs.yml)"
    echo "  --enclave NAME      Enclave name (default: CB-Testnet)"
    echo "  --package PATH      ethereum-package path or ref (default: ethpandaops/ethereum-package)"
    echo "  --keep              Don't tear down the enclave on exit"
    echo "  --json              Output JSON report"
    echo "  --timeout SECS      Readiness timeout (default: 1500)"
    echo "  --min-epochs N      Observation window in epochs (default: 2)"
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
        --timeout)    TIMEOUT="$2"; shift 2;;
        --min-epochs) MIN_EPOCHS="$2"; shift 2;;
        -v|--verbose) VERBOSE="-v"; shift;;
        -h|--help)    usage;;
        *)            echo "Unknown option: $1"; exit 2;;
    esac
done

# Default config
if [[ -z "$CONFIG" ]]; then
    CONFIG="$REPO_DIR/configs/basic-pbs.yml"
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
export PYTHONPATH="${REPO_DIR}/src${PYTHONPATH:+:$PYTHONPATH}"
python3 -m cb_verifier \
    --enclave "$ENCLAVE" \
    --timeout "$TIMEOUT" \
    --min-epochs "$MIN_EPOCHS" \
    $JSON_FLAG \
    $VERBOSE
