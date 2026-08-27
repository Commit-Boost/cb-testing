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
# RESOURCE NOTE: cb-mux is the heaviest scenario (2 relays + 256 validators). On a
# constrained/shared box, --jobs 2 can CPU-starve it — register_validator deadline
# timeouts (555) + get_header 4xx + zero delivery is the starvation signature, NOT a
# defect. Re-run the offender solo (`just e2e configs/generated/cb-mux.yml`) or run
# the whole gate at `just sweep-gate 1` for a definitive (slower) result.
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

# ePBS (gloas) + commit-boost + keymanager loop.
# Stands up a gloas devnet with buildoor, adds commit-boost as a first-class
# kurtosis enclave service (the PBS sidecar), runs `cb-km apply`, and asserts
# builder-built blocks flow VC -> CB -> buildoor. One devnet at a time (~15G).
# See docs/EPBS.md (incl. the native-mev_type / submodule-upgrade blockers). Env
# knobs: CB_LAUNCH (service|docker), OBSERVE_SLOTS, KEEP=1, CB_KM_BIN, CB_IMAGE.
# PREREQ: local/lodestar:km + the CB km-e2e image + cb-km binary.
epbs-sim:
    ./scripts/run-epbs-sim.sh

# ePBS sim with an opt-in feature-level regression assertion.
#   just epbs-sim-assert p2p               min_bid p2p floor rejects buildoor's p2p bid.
#   just epbs-sim-assert preserve          --preserve-entries keeps a third-party entry.
#   just epbs-sim-assert block-submission  CB's POST /beacon_blocks reveal endpoint fired.
#   just epbs-sim-assert builder-down      buildoor stop never stalls the proposer.
#   just epbs-sim-assert request-auth      verify_builder_request_auth ON, 0 AuthSigVerify.
epbs-sim-assert mode:
    ./scripts/run-epbs-sim.sh --assert {{mode}}

# Assert CB's POST /eth/v1/builder/beacon_blocks reveal endpoint fired (>=1 block
# accepted, no 5xx). Runs the full builder loop, then checks the reveal path.
epbs-sim-block-submission:
    ./scripts/run-epbs-sim.sh --assert block-submission

# Assert builder failure never stalls the proposer: after buildoor builds >=1
# block it is stopped, and the chain must keep advancing with self-built blocks
# while CB returns 204 (never 500).
epbs-sim-builder-down:
    ./scripts/run-epbs-sim.sh --assert builder-down

# Assert the request-auth signing domain agrees end to end: render CB with
# verify_builder_request_auth = true and require builder-built blocks with zero
# AuthSigVerify / 401 on the bid endpoint.
epbs-sim-request-auth:
    ./scripts/run-epbs-sim.sh --assert request-auth

# Cross-client gloas builder-flow coverage via assertoor (geth x lodestar/lighthouse/
# teku/nimbus/grandine, minimal preset). This is the VC -> buildoor DIRECT path with
# NO commit-boost in the loop: it answers "which CLs correctly implement the devnet-8
# gloas builder flow", complementing the CB-in-loop epbs-sim asserts above. assertoor
# reports per-playbook pass/fail (UI + HTTP API + process exit code); dora gives a
# block explorer. See configs/epbs/gloas-epbs-matrix.yaml.
epbs-matrix:
    kurtosis run github.com/ethpandaops/ethereum-package \
      --enclave epbs-matrix \
      --args-file configs/epbs/gloas-epbs-matrix.yaml \
      --image-download always

# Print the assertoor + dora URLs for a running epbs-matrix enclave.
epbs-matrix-urls:
    kurtosis enclave inspect epbs-matrix | grep -iE 'assertoor|dora'
