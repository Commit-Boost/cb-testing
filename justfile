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
    cargo run --release --bin cb-verify -- \
        --enclave {{enclave}} \
        --config {{config}} \
        --min-epochs 0 \
        --timeout 300

# Generate Kurtosis YAML configs into configs/generated/ (the typed `sim`
# generator). Loads optional .env for Docker image overrides (see .env.example).
generate-configs:
    cargo run --quiet --bin sim -- generate

# Build the Commit-Boost image the devnet runs, from the bundled commit-boost submodule
# (default ./commit-boost-client submodule). Produces commit-boost/commit-boost:{{tag}};
# keep it in sync with MEV_BOOST_IMAGE in .env.
build-cb-image tag="kurtosis" cb_dir="./commit-boost-client":
    cd {{cb_dir}} && just build-all {{tag}}

# Build the helix relay image from the bundled ./helix submodule. REQUIRED for the
# ws header-stream scenarios: the public ghcr.io/gattaca-com/helix-relay:main image
# STUBS the header-stream admission (admit_header_stream returns "header stream not
# available"; the real logic is in gattaca's private ApiProvider), so the stream is
# refused for every proposer. The `develop` submodule still carries the working public
# admission. Point HELIX_RELAY_IMAGE at this tag in .env to run a ws scenario.
# Produces local/helix-relay:{{tag}}.
build-helix-image tag="kurtosis":
    docker build -t local/helix-relay:{{tag}} -f helix/relay.Dockerfile helix/

# Pre-pull the public images the devnet needs so `kurtosis run` doesn't stall.
# (The CB sidecar image is built locally — see build-cb-image.)
pull-images:
    docker pull ghcr.io/gattaca-com/helix-relay:main
    docker pull ethpandaops/reth-rbuilder:develop
    docker pull sigp/lighthouse:latest

# One-command e2e: (re)generate configs, pull public images, launch + verify.
# PREREQ (once): `just build-cb-image` — the CB image must exist locally.
# Usage: just e2e            (cb-basic)
#        just e2e configs/generated/cb-mux.yml
e2e config="configs/generated/cb-basic.yml": generate-configs pull-images
    just testnet {{config}}

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

# The MEV-delivery GATE: run the core "green vegetable" scenarios with a fast
# window (wait 1 epoch, observe 1 epoch, skip finalization) — all must pass (exit 0).
# This is the manual gate (a full devnet OOMs free GitHub runners, so there is no
# nightly CI for it). Excludes: cb-sigverify-diff-control (poison negative control,
# fails by design), cb-ws-stream* (need a submodule-built helix — see
# build-helix-image + docs/composable-scenarios.md), cb-signer.
# NOTE run `just build-cb-image` once first (the CB image must exist).
sweep-gate jobs="2": generate-configs pull-images
    cargo run --release --bin cb-orchestrator -- \
        --jobs {{jobs}} --target-epoch 1 --min-epochs 1 --skip-finalization \
        configs/generated/cb-basic.yml \
        configs/generated/cb-basic-nethermind-prysm.yml \
        configs/generated/cb-multiple-relays.yml \
        configs/generated/cb-mux.yml \
        configs/generated/cb-skip-sigverify.yml \
        configs/generated/cb-sigverify-diff.yml \
        configs/generated/cb-timing-games.yml \
        configs/generated/cb-extra-validation.yml \
        configs/generated/cb-config-surface.yml \
        configs/generated/cb-min-bid.yml
