# cb-testnet-verifier

Automated verification for [Commit-Boost](https://github.com/Commit-Boost/commit-boost-client) Kurtosis devnets. Spins up a local Ethereum testnet with Commit-Boost as the MEV sidecar, waits for the MEV pipeline to stabilize, verifies each stage of the pipeline, and reports pass/fail.

## What is this?

When Commit-Boost proxies MEV between validators and relays, several things need to work in sequence: validators register with the relay, the builder submits blocks, the relay serves headers through CB, and the delivered payloads land on chain. If any stage breaks, the rest falls apart silently.

This tool verifies each stage independently and tells you exactly what broke.

```
Validator -> CB (get_header) -> Relay -> Builder
Validator -> CB (submit_block) -> Relay -> Payload on chain
                                            ^
                                    we verify this whole flow
```

## Prerequisites

- [Kurtosis CLI](https://docs.kurtosis.com/install) (>= 0.90)
- Python 3.10+
- [uv](https://github.com/astral-sh/uv) (recommended) or pip
- Docker (for Kurtosis)

If you're testing a local CB build, you also need:
- The [commit-boost-client](https://github.com/Commit-Boost/commit-boost-client) repo cloned
- Rust toolchain (to build the PBS image)

## Quick start

```bash
# Set up Python environment
uv venv
source .venv/bin/activate
uv pip install requests

# Run with the default config (2 Lighthouse/Geth nodes, 6s slots)
./scripts/run-and-verify.sh

# Takes ~40 minutes. You'll see:
#   [PASS] chain_finality - Finalized epoch: 5
#   [PASS] relay.builder_blocks_received - Received 14373 builder block(s)
#   [PASS] relay.payloads_delivered_multi - Delivered 64 payload(s)
#   [PASS] payload_hash_match - 64 matched, 0 mismatched
#   Result: PASS  (8 passed, 0 failed, 0 warnings)
```

### Testing a local CB build

```bash
# In the commit-boost-client repo, build the PBS Docker image:
just build-pbs kurtosis

# Then run the verifier (it uses the commit-boost/pbs:kurtosis image):
./scripts/run-and-verify.sh --config configs/basic-pbs.yml
```

### Testing against a specific ethereum-package

If you have a local checkout of the [ethereum-package](https://github.com/ethpandaops/ethereum-package) (e.g., with the custom CB config PR):

```bash
./scripts/run-and-verify.sh --package ../ethereum-package --keep -v
```

The `--keep` flag leaves the enclave running so you can inspect it after. The `-v` flag enables debug logging.

## What it checks

### Tier 1: Pipeline health (must pass)

| Check | What it verifies |
|---|---|
| `chain_finality` | Beacon chain has finalized (epoch >= 2) |
| `sync_status` | Beacon node is not syncing |
| `cb_running` | Commit-Boost services are running in the enclave |
| `relay.builder_blocks_received` | The builder submitted blocks to the relay |
| `relay.payloads_delivered_multi` | The relay delivered payloads to proposers |
| `payload_hash_match` | Every delivered payload's block_hash matches on chain |

### Tier 2: Quality metrics (should pass)

| Check | What it verifies | Threshold |
|---|---|---|
| `missed_slots` | Missed slot rate in observation window | < 10% |
| `relay.mev_delivery_rate` | Slots using relay-built blocks vs local | >= 30% |
| `cb_relay_latency` | Mean get_header latency | < 500ms |
| `cb_relay_errors` | Relay 5xx error count | == 0 |
| `cb_header_values` | Relay header bid values | > 0 |
| `cb_get_header_success` | Successful get_header responses | > 0 |

Tier 2 metric checks (latency, errors, header values, get_header success) require CB metrics. See [Metrics limitations](#metrics-limitations) below.

### Tier 3: Extended checks (config-dependent)

| Check | When it runs |
|---|---|
| `relay.validator_registrations` | When validator pubkeys are provided |

Mux routing, SSZ encoding, and signer health checks are planned but not yet implemented.

## Config presets

The `configs/` directory contains ready-to-use Kurtosis config files:

| Config | Nodes | What it exercises |
|---|---|---|
| [`basic-pbs.yml`](configs/basic-pbs.yml) | 2x Lighthouse/Geth | Default PBS pipeline. Header selection, relay fan-out. Start here. |
| [`pbs-metrics.yml`](configs/pbs-metrics.yml) | 2x Lighthouse/Geth | PBS + `[metrics]` enabled + Prometheus. Exercises Tier 2 checks. |
| [`pbs-validation-modes.yml`](configs/pbs-validation-modes.yml) | 3x Lighthouse/Geth | Mux with different `header_validation_mode` settings (None vs Standard). |

All presets use `commit_boost_config` to inject a full CB config as inline TOML. Template variables (`{{ .Network }}`, `{{ .Port }}`, `{{ .Relays }}`, `{{ .Timestamp }}`) are injected by the ethereum-package at launch.

Additional reference configs for SSZ testing, client matrix testing, and mux experiments are in `configs/reference/`.

### Writing your own config

Start from `basic-pbs.yml` and modify the `commit_boost_config` block. The config is standard CB TOML with template variable injection. Key things to know:

- `{{ .Port }}` becomes the PBS listen port (18550)
- `{{ .Relays }}` is the list of relay URLs discovered by the ethereum-package
- `{{ .Network }}` is the path to the network config inside the container
- `{{ .Timestamp }}` is the genesis time
- `spamoor` in `additional_services` is required (generates transactions so the builder has something to build)

## CLI reference

### cb-verify

```
python3 -m cb_verifier --enclave CB-Testnet [OPTIONS]

Options:
  --enclave NAME        Kurtosis enclave name (required)
  --min-epochs N        Observation window in epochs (default: 2)
  --target-epoch N      Wait until this epoch before checks (default: 5)
  --timeout SECS        Readiness timeout in seconds (default: 1500)
  --mev-threshold PCT   Min MEV delivery rate, 0.0-1.0 (default: 0.30)
  --json                Output JSON report
  -v, --verbose         Debug logging
```

### run-and-verify.sh

```
./scripts/run-and-verify.sh [OPTIONS]

Options:
  --config FILE         Kurtosis config (default: configs/basic-pbs.yml)
  --enclave NAME        Enclave name (default: CB-Testnet)
  --package PATH        ethereum-package: local path or GitHub ref
  --keep                Don't tear down the enclave on exit
  --json                JSON output
  --timeout SECS        Readiness timeout (default: 1500)
  --min-epochs N        Observation window (default: 2)
  -v, --verbose         Debug logging
```

## How it works

1. **Discovery**: Finds beacon, relay, and CB services via `kurtosis enclave inspect`
2. **Readiness**: Polls beacon API until synced, finalizing, and past epoch 5 (no hardcoded sleeps)
3. **Observation**: Watches 2 more epochs of steady-state activity
4. **Checks**: Queries beacon API, relay data API, and (optionally) CB metrics
5. **Report**: Colored terminal output or JSON (`--json`). Exit code 0/1/2.

### Exit codes

- `0` All Tier 1 checks passed
- `1` One or more Tier 1 checks failed
- `2` Setup failure (enclave not found, timeout, etc.)

### Timing

With the default `seconds_per_slot: 12` (matching mainnet), expect ~40 minutes total:
- ~32 min for the devnet to reach epoch 5 and finalize
- ~8 min for the 2-epoch observation window

## Metrics limitations

CB's metrics server only starts when the `CB_METRICS_PORT` environment variable is set. This env var is normally injected by `cb docker init` when generating docker-compose files, but the ethereum-package doesn't set it.

The verifier attempts to fetch metrics via `kurtosis service exec` (curling localhost inside the container), but this will fail until the upstream issue is resolved. There's an open issue to fix this: the config file's `[metrics]` block should be sufficient to start the metrics server without the env var.

Tier 2 metric checks (latency, errors, header values) will show as SKIP until this is fixed. All Tier 1 checks work without metrics.

## Project layout

```
cb-verify              Main orchestrator: discovery, readiness, checks, report
discovery.py              Kurtosis service/port discovery via CLI
report.py                 Terminal (ANSI) and JSON output formatting
checks/
  chain_health.py         Finality, sync, missed slots, CB service status
  relay_pipeline.py       Builder blocks, delivered payloads, MEV delivery rate
  payload_matching.py     Cross-ref relay payloads with on-chain blocks
  cb_metrics.py           CB Prometheus metric assertions
configs/
  basic-pbs.yml           Default PBS preset
  pbs-metrics.yml         PBS + metrics preset
  pbs-validation-modes.yml  Mux validation mode preset
  reference/              Additional configs for SSZ, client matrix, etc.
scripts/
  run-and-verify.sh       Full lifecycle: launch, verify, tear down
pyproject.toml            Python project config (deps, entry point)
PLAN.md                   Design doc and roadmap
```
