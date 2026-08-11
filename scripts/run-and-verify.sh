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
REQUIRE_FEATURE_PROOF_FLAG=""
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
    echo "  --require-feature-proof  Fail when an armed tier-1 feature check proves nothing (Law 3)"
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
        --require-feature-proof) REQUIRE_FEATURE_PROOF_FLAG="--require-feature-proof"; shift;;
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

# Advisory: report host memory before a ~10-min run and warn if the box is
# genuinely tight (a truly exhausted host can stall the kurtosis launch or thrash
# the whole run). NOTE: this is NOT what killed the relays in the 2026-07-31 run —
# that was a PER-CONTAINER cgroup OOM (CONSTRAINT_MEMCG) at the relays' own
# RELAY_MAX_MEMORY cap, fixed by raising that cap in the ethereum-package
# launchers, independent of host memory. Non-blocking; set LOW_MEM_ABORT=1 to abort.
check_host_memory() {
    local need_mb=24000  # ~2x8GB relays + ~10 services + headroom
    local avail_mb swap_total_mb swap_free_mb swap_used_mb
    avail_mb=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo)
    swap_total_mb=$(awk '/SwapTotal/ {print int($2/1024)}' /proc/meminfo)
    swap_free_mb=$(awk '/SwapFree/ {print int($2/1024)}' /proc/meminfo)
    swap_used_mb=$(( swap_total_mb - swap_free_mb ))
    echo "Host memory: ${avail_mb}MB available; swap ${swap_used_mb}/${swap_total_mb}MB used."
    if (( avail_mb < need_mb )); then
        echo "" >&2
        echo "⚠️  HOST MEMORY LOW — a devnet wants ~${need_mb}MB available; the launch may stall" >&2
        echo "    or the run may thrash. Top memory consumers to consider freeing:" >&2
        ps -eo rss,comm --sort=-rss 2>/dev/null | awk 'NR>1 && NR<=6 {printf "      %5.1f GB  %s\n", $1/1024/1024, $2}' >&2
        if [[ "${LOW_MEM_ABORT:-0}" == "1" ]]; then
            echo "    Aborting (LOW_MEM_ABORT=1)." >&2
            exit 2
        fi
        echo "    Proceeding anyway (set LOW_MEM_ABORT=1 to abort instead)." >&2
        echo "" >&2
    fi
}

# Step 0: Pre-build the verifier BEFORE the devnet is up — a `cargo run --release`
# on cb-verify is a multi-GB compile; keeping it off the critical path (it used to
# run mid-devnet) means the box isn't compiling while 10 services are live. Compile
# while idle, then invoke the built binary.
echo "Building cb-verify (release) before launch..."
cargo build --release --bin cb-verify --manifest-path "$REPO_DIR/Cargo.toml"
CB_VERIFY_BIN="$REPO_DIR/target/release/cb-verify"

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

# Step 1c: Host memory check (warn before spending ~10min on a run the OOM-killer
# would wreck).
check_host_memory

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

# Step 3: Run verification (pre-built binary from Step 0 — no mid-devnet compile).
"$CB_VERIFY_BIN" \
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
    $REQUIRE_FEATURE_PROOF_FLAG \
    $VERBOSE
