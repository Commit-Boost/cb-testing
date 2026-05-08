# cb-testnet-verifier

Automated verification for [Commit-Boost](https://github.com/Commit-Boost/commit-boost-client) Kurtosis devnets. Spins up a local Ethereum testnet with Commit-Boost as the MEV sidecar, waits for the MEV pipeline to stabilize, verifies each stage of the pipeline, and reports pass/fail.

## Prerequisites

- [Kurtosis CLI](https://docs.kurtosis.com/install) (>= 0.90)
- [Rust toolchain](https://rustup.rs/) (1.91+)
- Docker (for Kurtosis)

If you're testing a local CB build, you also need:
- The [commit-boost-client](https://github.com/Commit-Boost/commit-boost-client) repo cloned

## Docker image configuration

The generated configs embed Docker images for the relay, PBS sidecar, builder CL,
and builder EL. These are hardcoded by default but can be overridden via `.env`:

```bash
# From the cb-testing/ directory:
cp .env.example .env
# Edit .env to point at your local images
```

| Variable | Default | Purpose |
|---|---|---|
| `HELIX_RELAY_IMAGE` | `helix-relay:kurtosis` | Custom Helix relay image |
| `MEV_RELAY_IMAGE` | `ethpandaops/mev-boost-relay:main` | mev-boost relay (multi-relay scenarios) |
| `MEV_BOOST_IMAGE` | `commit-boost/pbs:kurtosis` | Commit-Boost PBS image |
| `BUILDER_CL_IMAGE` | `sigp/lighthouse:latest` | Builder consensus client |
| `BUILDER_EL_IMAGE` | `ethpandaops/reth-rbuilder:develop` | Builder execution client |

The `.env` file is read automatically by `generate_kurtosis_configs.py`.
It is gitignored — do not commit it. Use `.env.example` as the reference.

## Kurtosis setup / gotchas

The repo contains a forked `ethereum-package` as a submodule:

```bash
git submodule update --init
```

The fork generalizes hardcoded patterns from upstream, enabling configs like commit-boost + helix that weren't possible before. Once [this PR](https://github.com/ethpandaops/ethereum-package/pull/1384) merges we can deprecate the fork.

### Kurtosis configs

Kurtosis uses a default Commit-Boost config that can be overridden by inlining it into the kurtosis config — see `configs/example-kurtosis-config.yml`. Every generated test config uses this pattern.

`generate_kurtosis_configs.py` generates test scenarios from `.env`:

```bash
just generate-configs
```

Six scenarios are generated:

| Config | What it tests |
|---|---|
| `cb-basic.yml` | Single relay (helix), default CB config |
| `cb-multiple-relays.yml` | Two relays (helix + flashbots), aggregated bidding |
| `cb-mux.yml` | Mux routing — 128 validators to helix, 128 to flashbots |
| `cb-skip-sigverify.yml` | Fast path with BLS signature verification disabled |
| `cb-timing-games.yml` | Aggressive per-relay timing overrides for late bidding |
| `cb-extra-validation.yml` | Extra get_header validation via local EL RPC |

## Quick start

```bash
# Generate configs from .env
just generate-configs

# Launch a testnet + verify (attached mode)
just testnet configs/generated/cb-mux.yml

# Verify a running enclave (standalone, no observation window)
just verify-now CB-Testnet

# Verify with mux routing checks
just verify-with-config configs/generated/cb-mux.yml CB-Testnet

# Show raw CB PBS logs for debugging
just show-logs CB-Testnet

# Quick mux routing diagnostic
just test-mux CB-Testnet configs/generated/cb-mux.yml

# Test relay API endpoints
cargo run --release --bin test-relay -- http://127.0.0.1:PORT 128 160
```

## What it checks

### Tier 1: Pipeline health (must pass)

| Check | What it verifies |
|---|---|
| `chain_finality` | Beacon chain has finalized (epoch >= 2) |
| `sync_status` | Beacon node is not syncing |
| `cb_running` | Commit-Boost services are running in the enclave |
| `relay.payloads_delivered_multi` | Relays delivered payloads to proposers |
| `payload_hash_match` | Delivered payload block_hashes match on-chain |
| `mux.routing` | Validator pubkeys routed to correct relay (config-dependent) |

### Tier 2: Quality metrics (should pass)

| Check | What it verifies | Threshold |
|---|---|---|
| `missed_slots` | Missed slot rate in observation window | < 10% |
| `relay.builder_blocks_received` | Builder submitted blocks to relay | > 0 |
| `relay.mev_delivery_rate` | Slots using relay-built blocks vs local | >= 30% |
| `relay.validator_registrations` | All validators registered on relay | 100% |

### Tier 3: CB metrics 

| Check | What it verifies |
|---|---|
| `cb_get_header_matrix` | get_header status codes from relay vs beacon side |
| `cb_register_validator_matrix` | register_validator status codes |
| `cb_submit_blinded_block_matrix` | submit_blinded_block status codes |
| `cb_status_matrix` | status check responses |
| `cb_v2_fallback` | v1→v2 fallback behavior |
| `cb_relay_latency` | get_header latency histogram (p95) |

## CLI reference

### cb-verify

```
cb-verify [OPTIONS]

Options:
      --enclave <NAME>        Kurtosis enclave name (required)
      --config <PATH>         Kurtosis YAML config (for mux verification)
      --min-epochs <N>        Observation window in epochs [default: 2]
      --target-epoch <N>      Wait before starting checks [default: 7]
      --timeout <SECS>        Readiness timeout [default: 3600]
      --mev-threshold <RATE>  Min MEV delivery rate [default: 0.30]
      --json                  Output JSON report
      --verbose               Debug logging
      --strict                Promote WARN to FAIL
      --live-metrics          Poll :9090/metrics during observation
      --show-logs             Print raw CB PBS logs, no checks
      --output-dir <DIR>      Save JSON reports (requires --json)
```

### test-mux

```
test-mux <enclave> <config>

Fetches CB PBS logs, parses mux events, verifies routing against config.
No observation window. Completes in seconds.
```

### test-relay

```
test-relay <relay_url> <start_slot> <end_slot> [pubkey]

Tests relay data API endpoints with slot filtering.
Verifies delivered payloads, builder blocks, validator registration.
```

## How it works

**Attached mode** (`just testnet`):
1. `run-and-verify.sh` launches Kurtosis enclave with the chosen config
2. `cb-verify` waits for chain readiness (target epoch, finalization)
3. Observes for `min_epochs` while polling health
4. Runs all tier checks, outputs report

**Standalone mode** (`just verify-now`):
1. Discovers services in running enclave via `kurtosis enclave inspect`
2. Skips observation window (`--min-epochs 0`)
3. Runs checks against current state

**Mux verification** (`test-mux` or `--config`):
1. Parses Kurtosis YAML → extracts `commit_boost_config` → finds `[[mux]]` sections
2. Fetches CB PBS logs via `kurtosis service logs`
3. Parses log lines (ANSI-aware) for `using mux config` events
4. Cross-references proposer pubkeys against mux mapping
5. FAIL if any pubkey appears on wrong relay

**Metrics** (tier 3):
1. Discovers metrics URL from `kurtosis port print` (port 9090)
2. Falls back to `kurtosis exec` into CB container
3. Parses Prometheus text format
4. Checks status code matrices, latency histograms

## Project layout

```
cb-testing/
  Cargo.toml              # Workspace: cb-verify, test-mux, test-relay
  justfile                # Build/test/launch commands
  README.md
  .env.example            # Docker image overrides
  scripts/
    run-and-verify.sh     # Attached mode launcher
    generate_kurtosis_configs.py  # Config generator
  configs/
    generated/            # Pre-generated test scenarios
    example-kurtosis-config.yml
  src/
    main.rs               # cb-verify binary
    checks/
      chain_health.rs     # Finality, sync, missed slots
      relay_pipeline.rs   # Delivery, registration, MEV rate
      payload_matching.rs # Hash matching
      mux_routing.rs      # Mux config parsing, log analysis
      cb_metrics.rs       # Prometheus metrics checks
    bin/
      test_mux.rs         # Mux diagnostic binary
      test_relay.rs       # Relay API diagnostic binary
  ethereum-package/       # Forked Kurtosis package (submodule)
```
