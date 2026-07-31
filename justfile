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

# Build the orchestrator binary (concurrent multi-enclave test runner)
build-orchestrator:
    cargo build --release --bin cb-orchestrator

# Run verifier against a running enclave
verify enclave="CB-Testnet" target_epoch="7" min_epochs="2":
    cargo run --release --bin cb-verify -- \
        --enclave {{enclave}} \
        --target-epoch {{target_epoch}} \
        --min-epochs {{min_epochs}} \
        --timeout 3600

# Run verifier with live metrics and strict mode
verify-strict enclave="CB-Testnet" target_epoch="7" min_epochs="2":
    cargo run --release --bin cb-verify -- \
        --enclave {{enclave}} \
        --target-epoch {{target_epoch}} \
        --min-epochs {{min_epochs}} \
        --timeout 3600 \
        --live-metrics \
        --strict

# Standalone: quick health check (no observation window)
verify-now enclave="CB-Testnet":
    cargo run --release --bin cb-verify -- \
        --enclave {{enclave}} \
        --min-epochs 0 \
        --timeout 60

# Standalone: verify with config (mux checks + health)
verify-with-config config enclave="CB-Testnet":
    cargo run --release --bin cb-verify -- \
        --enclave {{enclave}} \
        --config {{config}} \
        --min-epochs 1 \
        --timeout 300

# Show raw CB PBS logs with parsing (for debugging)
show-logs enclave="CB-Testnet":
    cargo run --release --bin cb-verify -- \
        --enclave {{enclave}} \
        --show-logs

# Quick mux routing check (no observation window, just fetch logs and check)
test-mux enclave="CB-Testnet" config="configs/generated/cb-mux.yml":
    cargo run --release --bin test-mux -- {{enclave}} {{config}}

# Generate Kurtosis YAML configs into configs/generated/ (the typed `sim`
# generator). Loads optional .env for Docker image overrides (see .env.example).
generate-configs:
    cargo run --quiet --bin sim -- generate

# Run kurtosis testnet with verification on target `config`.
# Observes 1 epoch starting at target_epoch. Chain just needs to reach
# end slot; checks query historical relay/beacon data (no real-time loop).
testnet config:
    ./scripts/run-and-verify.sh \
        --config {{config}} \
        --json \
        --live-metrics \
        --min-epochs 1 \
        --target-epoch 1 \
        --keep \
        --skip-finalization \
        -v

# Run a single config with verbose logging (for debugging)
testnet-verbose config:
    ./scripts/run-and-verify.sh \
        --config {{config}} \
        --json \
        --live-metrics \
        --min-epochs 1 \
        --target-epoch 2 \
        --keep \
        -v

# Run all generated configs concurrently and print a summary.
#
# Uses cb-orchestrator to run multiple enclaves in parallel (bounded by --jobs).
# Each config gets its own enclave. While one is observing, others can launch.
# For N configs with --jobs=4, expect roughly 4× throughput vs sequential.
#
# Usage:
#   just test-all                    # default: 2 jobs, no results dir
#   just test-all 4 /tmp/results    # 4 jobs, save results to /tmp/results
#   just test-all 2 /tmp/results --strict --keep
test-all jobs="2":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run --release --bin cb-orchestrator -- --jobs {{jobs}}

# Run a single config through the orchestrator (for debugging)
test-one config jobs="1":
    cargo run --release --bin cb-orchestrator -- \
        --jobs {{jobs}} \
        {{config}}
