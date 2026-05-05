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

# Generate Kurtosis YAML configs from templates into kurtosis-configs/
generate-configs:
    python3 scripts/generate_kurtosis_configs.py --output-dir kurtosis-configs/

# Verify that the generator reproduces ground truth exactly
verify-configs:
    #!/usr/bin/env bash
    set -euo pipefail
    TMPDIR=$(mktemp -d)
    python3 scripts/generate_kurtosis_configs.py --output-dir "$TMPDIR"
    for f in cb-basic cb-multiple-relays cb-skip-sigverify cb-timing-games cb-extra-validation cb-mux; do
        diff -q "$TMPDIR/${f}.yml" configs/generated/${f}.yml
    done
    echo "All configs match ground truth."
