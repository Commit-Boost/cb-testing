# cb-testing playbook

How to stand up the Commit-Boost devnet and run the verification suite. Written for a teammate
who has never touched this repo. If you want to *change* the harness (add a check or a scenario),
read [`DEVELOPING.md`](DEVELOPING.md) after this. If a run misbehaves, jump to
[Troubleshooting](#troubleshooting).

What this repo does: it clones a local Ethereum devnet with Commit-Boost as the MEV sidecar (helix
relay + reth-rbuilder + the CB PBS module), lets the MEV pipeline stabilize, checks each stage, and
prints a tiered pass/fail verdict. A tier-1 FAIL exits non-zero; that is the gate you care about.

---

## 0. One-time setup (~15 min, mostly the first CB build)

You need: **Docker**, the **Rust toolchain** (1.91+, edition 2024), the **Kurtosis CLI pinned to
1.18.1** (a newer CLI writes an incompatible config and the log parsers break — see
[`local-kurtosis-e2e.md`](local-kurtosis-e2e.md)), and **`just`** (`cargo install just`).

```bash
# 1. Clone WITH submodules. This is the whole testing ground in one step:
#    ethereum-package (upstream + patch branch), commit-boost-client (CB source), helix (relay source).
git clone --recursive https://github.com/Commit-Boost/cb-testing.git
cd cb-testing
#    Already cloned without --recursive?  git submodule update --init

# 2. Build the CB sidecar image the devnet runs. Long first compile (full Rust
#    release build in docker). Produces commit-boost/commit-boost:kurtosis.
just build-cb-image

# 3. Sanity-check the harness itself compiles and its unit tests pass.
just ci
```

You do **not** build helix. Every scenario, websocket bid stream included, runs the published helix
`develop` image pinned by digest; see
[Testing the websocket header stream](#testing-the-websocket-header-stream). The submodule is there
for building a custom relay branch (see
[Testing a specific branch](#testing-a-specific-cb-or-helix-branch)).

---

## 1. Run one scenario end to end

```bash
just e2e                                   # default scenario: cb-basic
just e2e configs/generated/cb-mux.yml      # a specific scenario
```

`e2e` regenerates configs, pulls the public images, launches the enclave, observes one epoch, runs
every check, prints the tiered report, and tears down. Exit codes:

| Code | Meaning |
|---|---|
| `0` | PASS — no tier-1 check failed |
| `1` | **tier-1 FAIL** — the MEV pipeline is broken (this is the gate) |
| `2` | setup failure — the devnet never came up (see Troubleshooting) |

Only a tier-1 FAIL is fatal. Tier-2 WARN and SKIP are informational; a check that is armed but could
not gather evidence reports `inconclusive` rather than a false PASS. For the check catalog and what
each tier means, see [`CHECKS.md`](CHECKS.md).

Add `--json` to the verifier for the machine-readable verdict (what CI consumes).

---

## 2. Run the whole scenario sweep

```bash
just sweep-gate                   # THE GATE: the core green MEV scenarios, all must pass
just test-all                     # every generated config, 2 in parallel
just test-all 4 /tmp/cb-results   # 4 in parallel, write per-scenario JSON to the dir
```

**`just sweep-gate` is the release gate.** It runs the core "green vegetable" scenarios (basic,
alt-clients, multiple-relays, mux, skip-sigverify, sigverify-diff, timing-games, extra-validation,
config-surface, min-bid) plus the four websocket bid-stream scenarios (`cb-ws-stream`,
`cb-ws-stream-filekey`, `cb-ws-prysm`, and the `cb-ws-stream-nokey` negative control) with a fast
window (wait 1 epoch, observe 1, skip finalization), and must exit 0. A full devnet OOMs free-tier
GitHub runners, so there is no nightly CI for this; the sweep is the gate. Fourteen configs at
`--jobs 2` is roughly 1h25m of wall clock. It deliberately excludes `cb-sigverify-diff-control` (the
poison control, which fails by design) and `cb-signer`.

The ws scenarios are real coverage only because `feature.ws_stream_served` is tier 1: commit-boost
falls back to HTTP on a failed handshake and keeps the slot green, so every other check passes on a
stream that served nothing. That check FAILs the run instead. `cb-ws-stream-nokey` configures the
relay to REFUSE the handshake and is in the gate on purpose: a control that never runs proves
nothing. See [`CHECKS.md`](CHECKS.md#the-websocket-bid-stream-checks-config-gated-on-get_header--stream).

`test-all` / `cb-orchestrator` drive each config through its own enclave, up to `--jobs` at a time.
Two flags matter for a fast, trustworthy sweep: `--target-epoch 1 --min-epochs 1` (observe a full epoch
window — without a proper window the MEV-delivery check measures a single slot and passes/fails by luck)
and `--skip-finalization` (don't wait for the chain to finalize, ~epoch 4+). The default `target-epoch`
is high so finalization passes without that flag, but it is much slower.

The scenarios live in `configs/generated/` (regenerate with `just generate-configs`). The README
table lists what each one exercises.

---

## 3. Verify against an enclave you already launched

If you brought a devnet up yourself (or `e2e` left one running with `--keep`) and just want to
re-run checks against it:

```bash
just verify           enclave="CB-Testnet"    # observe, then the tiered report
just verify-strict    enclave="CB-Testnet"    # + live metrics, strict mode
just verify-now       enclave="CB-Testnet"    # quick health check, no observation window
just show-logs        enclave="CB-Testnet"    # raw CB PBS logs, parsed (debugging)
```

---

## Testing the websocket header stream

Nothing to set up: the baked default relay image is helix `develop`, pinned by digest, and it serves
the stream. `main` carries no `header_stream` route at all, which is why `develop` is the default for
every scenario rather than a ws-only override.

```bash
just e2e configs/generated/cb-ws-stream.yml             # or any get_header=stream compose
```

Expected: `feature.ws_stream_served` PASS (the tier-1 gate), `feature.ws_header_stream` PASS, and zero
or one startup-race HTTP fallback. Note: `relay.validator_registrations` SKIPs against `develop` (its
data-api query is unpopulated in this devnet; the check confirms registration via delivery instead).

`cb-ws-stream-nokey` is the negative control: it sets `header_stream.admit_all: false`, so the relay
refuses every handshake and the run falls back to HTTP. Its correct outcome is
`feature.ws_stream_served` PASS with `feature.ws_header_stream` WARN-inconclusive, and it is expected
to fail under `--require-feature-proof` for exactly that reason.

---

### Proving the relay saw a file-sourced api key (`cb-ws-stream-filekey`)

`cb-ws-stream-filekey` delivers the ws api key as a secret file and proves CB
read it (`feature.relay_header_file`, tier 1). The relay-side half,
`feature.relay_saw_api_key`, is tier 2 and stays inconclusive on any stock
helix: upstream never logs the key it receives. To make it conclusive, build
the helix image with one extra field on the stream-admission line and point
`HELIX_RELAY_IMAGE` at it:

```rust
// helix/crates/relay/src/api/proposer/header_stream.rs, the `accepting header stream` info!
x_api_key = headers.get(HEADER_API_KEY).and_then(|v| v.to_str().ok()).unwrap_or(""),
```
(`HEADER_API_KEY` is `helix_common::api::HEADER_API_KEY`.) Then
`just build-helix-image apikey-log` and `HELIX_RELAY_IMAGE=local/helix-relay:apikey-log`.
The check compares that value byte-for-byte with the file the ethereum-package
rendered from `commit_boost_extra_files`.

## Testing a specific CB or helix branch

The build sources are submodules, so you switch branches inside them and rebuild — the same loop
the retired ws-workspace gave us.

```bash
# A CB branch (e.g. a PR you are reviewing):
cd commit-boost-client && git fetch origin && git checkout <branch> && cd ..
just build-cb-image                 # rebuilds commit-boost/commit-boost:kurtosis from it
just e2e                            # run against it

# A helix branch: build the relay image locally, then point .env at it.
cd helix && git fetch origin && git checkout <branch>
docker build -f relay.Dockerfile -t helix-relay:local . && cd ..
cp .env.example .env                # if you have not already
#   set HELIX_RELAY_IMAGE=helix-relay:local in .env, then:
just e2e
```

`.env` overrides every image the configs embed (`HELIX_RELAY_IMAGE`, `MEV_BOOST_IMAGE`,
`BUILDER_EL_IMAGE`, ...); it is gitignored — see `.env.example` and the README image table. To go
back to the pinned known-good CB, `cd commit-boost-client && git checkout main` (the submodule ships
pinned to a certified `main`) and rebuild.

---

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| Exit `2`, devnet never stabilized | Kurtosis CLI is not **1.18.1**. Check `kurtosis version`; a 1.20.x config is incompatible. See [`local-kurtosis-e2e.md`](local-kurtosis-e2e.md). |
| `just build-cb-image` can't find the source | Submodules not initialized: `git submodule update --init`. |
| `kurtosis run` stalls pulling images | Pre-pull first: `just pull-images`. |
| Checks report `inconclusive` | The check armed but couldn't gather evidence (e.g. no bid landed in the window). Not a pass and not a hard fail — read its line in the report; often a timing/scenario issue, not a CB bug. |
| Enclave left running after a crash | `kurtosis enclave rm -f <name>` (list with `kurtosis enclave ls`). |
| Need to kill a run | Never bare `pkill`. Stop the enclave with `kurtosis enclave rm -f`, or Ctrl-C the `just` process. |
| A sweep scenario FAILs with register_validator deadline timeouts (555) + get_header 4xx | Resource starvation, not a defect — the heaviest scenario (cb-mux) got CPU-starved under `--jobs 2` on a shared box. Re-run it solo (`just e2e configs/generated/cb-mux.yml`) or run the gate at `just sweep-gate 1`. |

For the design of the verdict model (why only tier-1 is fatal, what `inconclusive` means, how checks
are attributed to a fork) see [`ARCH.md`](ARCH.md) and [`DESIGN.md`](DESIGN.md).
