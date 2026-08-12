# cb-testnet-verifier

Automated verification for [Commit-Boost](https://github.com/Commit-Boost/commit-boost-client) Kurtosis devnets. Spins up a local Ethereum testnet with Commit-Boost as the MEV sidecar, waits for the MEV pipeline to stabilize, verifies each stage of the pipeline, and reports pass/fail.

## Prerequisites

- [Kurtosis CLI](https://docs.kurtosis.com/install) — **pin 1.18.1** (the proven-good version; the parsers
  rely on its text-table output, and a newer CLI, e.g. 1.20.0, writes an incompatible
  `~/.config/kurtosis/kurtosis-config.yml` — see `docs/local-kurtosis-e2e.md`)
- [Rust toolchain](https://rustup.rs/) (1.91+, edition 2024)
- Docker (for Kurtosis)
- The bundled submodules: clone with `--recursive`, or run `git submodule update --init` in an
  existing checkout. This pulls three: the forked `ethereum-package`, `commit-boost-client`
  (the CB sidecar source, built into the devnet image), and `helix` (relay source, for local
  branch-switching builds).

If you're testing a local CB build (the default — the CB sidecar image is built, not pulled), the
`commit-boost-client` submodule is where it builds from (overridable — see `just build-cb-image`).

> Contributing? See **[`docs/DEVELOPING.md`](docs/DEVELOPING.md)** for the dev loop and how to add checks + scenarios.

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
| `HELIX_RELAY_IMAGE` | `ghcr.io/gattaca-com/helix-relay:main` | Helix relay image |
| `MEV_RELAY_IMAGE` | `ethpandaops/mev-boost-relay:main` | flashbots mev-boost-relay — **no longer used** (multi-relay now runs two helix instances); still emitted into configs but inert |
| `MEV_BOOST_IMAGE` | `commit-boost/commit-boost:kurtosis` | Commit-Boost sidecar image |
| `BUILDER_CL_IMAGE` | `sigp/lighthouse:latest` | Builder consensus client |
| `BUILDER_EL_IMAGE` | `ethpandaops/reth-rbuilder:develop` | Builder execution client |

The `.env` file is read automatically by `just generate-configs` (the `sim generate`
command). It is gitignored — do not commit it. Use `.env.example` as the reference.

## Kurtosis setup / gotchas

Kurtosis stands the devnet up from a forked `ethereum-package`, one of the three bundled submodules
(init all three with `git submodule update --init`, or clone with `--recursive`):

| Submodule | Source | Role |
|---|---|---|
| `ethereum-package` | forked `Commit-Boost/ethereum-package` | the Kurtosis devnet definition |
| `commit-boost-client` | `Commit-Boost/commit-boost-client` | CB sidecar source, built into the devnet image |
| `helix` | `gattaca-com/helix` | relay source, for local branch-switching builds (image is pulled) |

The `ethereum-package` fork generalizes hardcoded patterns from upstream, enabling configs like commit-boost + helix that weren't possible before. Once [this PR](https://github.com/ethpandaops/ethereum-package/pull/1384) merges we can deprecate the fork.

### Kurtosis configs

Kurtosis uses a default Commit-Boost config that can be overridden by inlining it into the kurtosis config. Every generated test config uses this pattern: the helix and commit-boost configs are embedded as the two `|` block scalars under `mev_params`.

`sim generate` (via `just generate-configs`) builds the test scenarios, applying any `.env` image overrides:

```bash
just generate-configs
```

Six scenarios are generated:

| Config | What it tests |
|---|---|
| `cb-basic.yml` | Single relay (helix), default CB config |
| `cb-multiple-relays.yml` | Two helix relay instances, aggregated bidding |
| `cb-mux.yml` | Mux routing — 128 validators to helix-1, 128 to helix-2 |
| `cb-skip-sigverify.yml` | Fast path with BLS signature verification disabled |
| `cb-timing-games.yml` | Aggressive per-relay timing overrides for late bidding |
| `cb-extra-validation.yml` | Extra get_header validation via local EL RPC |

### Composable scenarios (`sim scenario`)

The named scenarios above are frozen points. To compose features freely — e.g. the
websocket stream on the prysm client pair with timing games, a combination no named
scenario covers — use `sim scenario`, which renders a `ScenarioSpec` through the same
assembly seams the goldens pin (so a rendered config is valid by construction):

```bash
# Start from a named base and apply typed field overrides:
cargo run --bin sim -- scenario \
  --base cb-basic --set get_header=stream,clients=nethermind-prysm,timing_games=true \
  --show-spec --out configs/generated/cb-ws-prysm-tg.yml

# Or supply a full ScenarioSpec as JSON (the AI-drivable surface; unknown keys rejected):
echo '{"topology":"mux"}' | cargo run --bin sim -- scenario --spec /dev/stdin
```

Overridable knobs: `clients` (geth-lighthouse | nethermind-prysm | geth-teku | geth-nimbus | geth-lodestar —
all 5 mainstream CLs), `topology`
(single | two-relays | divergent-relays | mux), `get_header` (http | stream | stream-nokey),
`sigverify` (on | skip | skip-poisoned | poisoned-control), `min_bid` (none | `<eth>`), and the
booleans `timing_games` / `extra_validation` / `signer`. `--show-spec` previews the resolved spec
and the features it arms. Design + rationale: [`docs/composable-scenarios.md`](docs/composable-scenarios.md).

## Quick start

```bash
# ONE-TIME (from scratch): init the bundled submodules, then build the
# Commit-Boost image the devnet runs (from the commit-boost-client submodule)
git submodule update --init   # or clone the repo with --recursive
just build-cb-image                 # -> commit-boost/commit-boost:kurtosis

# Generate configs, pull public images, launch + verify. Prints the tiered
# report and exits 0 (pass) / 1 (tier-1 FAIL) / 2 (setup failure). Add --json
# to the verifier for the machine-readable verdict (see docs/CHECKS.md).
just e2e                            # cb-basic
just e2e configs/generated/cb-mux.yml

# --- or the individual steps ---

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
```

## What it checks

> The tables below are a quick reference. **[`docs/CHECKS.md`](docs/CHECKS.md) is the authoritative
> catalog** — per-check pass/warn/fail contract, data source, and the load-bearing verdict rule: the
> process exit code keys **only on a tier-1 FAIL**; WARN and SKIP are non-fatal, so a consumer gating on
> a trust-critical anomaly must read the JSON `result:"WARN"`, not just the exit code.

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
| `relay.best_bid` | CB delivered >= the best bid it was offered across relays | competition + delivered |

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
      --skip-finalization-check  Skip the finality check (for early/short windows)
```

### sim (generate | preflight | triage)

```
sim generate [SCENARIO] [--out-dir DIR] [--check]   # typed Rust config generator
                                                    #   --check: verify on-disk configs match (CI drift gate)
sim preflight <ARGS_FILE>                           # validate the config against the real helix image (~1s)
                                                    #   exit 1 only on a genuine config-drift Fail
sim triage <ENCLAVE>                                # extract each dead service's root panic (JSON)
```

### test-mux

```
just test-mux <enclave> <config>

Runs cb-verify with --config against a running enclave: fetches CB PBS logs,
parses mux events, verifies routing against config. No observation window.
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
  Cargo.toml              # Workspace: cb-verify, cb-orchestrator, sim
  justfile                # Build/test/launch commands
  README.md
  .env.example            # Docker image overrides
  scripts/
    run-and-verify.sh     # Attached mode launcher (preflight-gated)
  configs/
    generated/            # Test scenarios, emitted by `sim generate`
  src/
    main.rs               # cb-verify binary
    bin/sim/              # sim: generate | preflight | triage
    checks/
      chain_health.rs     # Finality, sync, missed slots
      relay_pipeline.rs   # Delivery, registration, MEV rate
      payload_matching.rs # Hash matching
      mux_routing.rs      # Mux config parsing, log analysis
      cb_metrics.rs       # Prometheus metrics checks
  ethereum-package/       # Forked Kurtosis devnet definition (submodule)
  commit-boost-client/    # CB sidecar source, built into the devnet image (submodule)
  helix/                  # Relay source, for local branch-switching builds (submodule)
```
