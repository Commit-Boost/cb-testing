# cb-testnet-verifier

default: lint

# Fast compile check (no codegen)
check:
    cargo check --all-targets

# Run all unit tests
test:
    cargo test

# Auto-format code
fmt:
    cargo fmt

# Check formatting (CI mode)
fmt-check:
    cargo fmt --check

# Strict clippy (treat warnings as errors)
clippy:
    cargo clippy --all-targets -- -D warnings

# Lint checklist: format + clippy (run before commit)
lint: fmt-check clippy

# Full CI pipeline locally
ci: check test lint

# Build release binary
build-release:
    cargo build --release

# Run verifier against a running enclave
verify enclave="CB-Testnet" target_epoch="7" min_epochs="2":
    cargo run --release -- \
        --enclave {{enclave}} \
        --target-epoch {{target_epoch}} \
        --min-epochs {{min_epochs}} \
        --timeout 3600

# Run verifier with live metrics and strict mode
verify-strict enclave="CB-Testnet" target_epoch="7" min_epochs="2":
    cargo run --release -- \
        --enclave {{enclave}} \
        --target-epoch {{target_epoch}} \
        --min-epochs {{min_epochs}} \
        --timeout 3600 \
        --live-metrics \
        --strict

# Generate Kurtosis YAML configs from templates into configs/generated/
# Loads optional .env for Docker image overrides (see .env.example).
generate-configs:
    python3 scripts/generate_kurtosis_configs.py

# Run kurtosis testnet with verification on target `config`
testnet config:
    ./scripts/run-and-verify.sh \
        --config {{config}} \
        --json \
        --live-metrics \
        --min-epochs 1 \
        --target-epoch 2

# Run all generated configs sequentially and print a summary.
#
# Iterates configs/generated/*.yml, runs each via `testnet` with
# --json-dir so results are saved. After all complete, aggregates
# the JSON reports into a summary table.
test-all:
    #!/usr/bin/env bash
    set -euo pipefail
    RESULTS_DIR="$(mktemp -d)"
    SCRIPTS_DIR="$(dirname "$0")/scripts"
    CONFIGS_DIR="$(dirname "$0")/configs/generated"
    echo "Results dir: $RESULTS_DIR"
    echo ""
    for config in "$CONFIGS_DIR"/*.yml; do
        name="$(basename "$config" .yml)"
        # Derive a unique enclave name from the config filename
        enclave="CB-${name#cb-}"
        echo "[$(date +%H:%M:%S)] Running $name..."
        "$SCRIPTS_DIR/run-and-verify.sh" \
            --config "$config" \
            --enclave "$enclave" \
            --json-dir "$RESULTS_DIR" \
            --live-metrics \
            --min-epochs 2 \
            --target-epoch 2 \
            2>&1 | tail -1 || echo "  $name: FAILED (exit code $?)"
    done
    echo ""
    echo "========================================"
    echo "  Batch Summary"
    echo "========================================"
    for f in "$RESULTS_DIR"/*.json; do
        [ -f "$f" ] || continue
        name="$(basename "$f" .json)"
        result="$(jq -r '.result // "unknown"' "$f" 2>/dev/null || echo "parse-error")"
        passed="$(jq '[.checks[] | select(.status == "Pass")] | length' "$f" 2>/dev/null || echo "?")"
        failed="$(jq '[.checks[] | select(.status == "Fail")] | length' "$f" 2>/dev/null || echo "?")"
        warn="$(jq '[.checks[] | select(.status == "Warn")] | length' "$f" 2>/dev/null || echo "?")"
        echo "  $name: $result  (${passed}p / ${failed}f / ${warn}w)"
    done
    echo "========================================"
