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
| `MEV_BOOST_IMAGE` | `commit-boost/commit-boost:latest` | Commit-Boost latest image |
| `BUILDER_CL_IMAGE` | `sigp/lighthouse:latest` | Builder consensus client |
| `BUILDER_EL_IMAGE` | `ethpandaops/reth-rbuilder:develop` | Builder execution client |

The `.env` file is read automatically by `generate_kurtosis_configs.py`.
It is gitignored — do not commit it. Use `.env.example` as the reference.

## Kurtosis setup / gotchas
The repo contains a forked `ethereum-package` as a submodule (don't forget to call `git submodule update`). The fork generalizes a lot of the hardcoded patterns in the upstream repo, allowing us to run configurations like commit-boost + helix that weren't possible before. Once [this PR](https://github.com/ethpandaops/ethereum-package/pull/1384) is merged we can deprecate the fork but until then the commands use this as the default Kurtosis package.

### Kurtosis configs
Kurtosis uses a default Commit-Boost config that can be overridden by inlining it into the kurtosis config, see `example-kurtosis-config.yml`. This is how every test is expected to run.

To simplify things, the `generate_kurtosis_configs.py` script generates different test scenarios using the values from your `.env` file. These make good references if you need to adjust your Commit-Boost settings.

```bash
# Creates pre-generated kurtosis config files from .env params
just generate-configs
```

## Quick start

REDOOOOO (use just cmds)

## What it checks

### Tier 1: Pipeline health (must pass)

| Check | What it verifies |
|---|---|
| `chain_finality` | Beacon chain has finalized (epoch >= 2) |
| `sync_status` | Beacon node is not syncing |
| `cb_running` | Commit-Boost services are running in the enclave |
| `relay.payloads_delivered_multi` | The relay delivered payloads to proposers |
| `payload_hash_match` | Every delivered payload's block_hash matches on chain |

### Tier 2: Quality metrics (should pass)

| Check | What it verifies | Threshold |
|---|---|---|
| `missed_slots` | Missed slot rate in observation window | < 10% |
| `relay.builder_blocks_received` | Builder submitted blocks to relay | > 0 |
| `relay.mev_delivery_rate` | Slots using relay-built blocks vs local | >= 30% |
| `cb_relay_latency` | Mean get_header latency | < 500ms |
| `cb_relay_errors` | Relay 5xx error count | == 0 |
| `cb_header_values` | Relay header bid values | > 0 |
| `cb_get_header_success` | Successful get_header responses | > 0 |

Tier 2 metric checks (latency, errors, header values, get_header success) require CB metrics.

### Tier 3: Extended checks (config-dependent)
talk about muxes


## CLI reference

REDO


## How it works

REDOOOOO

## Project layout
REDOOOOOOOOOOOOOOOOO
