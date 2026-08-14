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
#    ethereum-package (forked), commit-boost-client (CB source), helix (relay source).
git clone --recursive https://github.com/Commit-Boost/cb-testing.git
cd cb-testing
#    Already cloned without --recursive?  git submodule update --init

# 2. Build the CB sidecar image the devnet runs. Long first compile (full Rust
#    release build in docker). Produces commit-boost/commit-boost:kurtosis.
just build-cb-image

# 3. Sanity-check the harness itself compiles and its unit tests pass.
just ci
```

For the default scenarios you do **not** build helix — its relay image is pulled public. **The websocket
header-stream scenarios (`cb-ws-stream`, `cb-ws-stream-nokey`, any `get_header=stream` compose) are the
exception**: the public `:main` image stubs the stream admission, so those need a helix built from the
bundled submodule — see [Testing the websocket header stream](#testing-the-websocket-header-stream). The
submodule is also there for building a custom relay branch (see
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
config-surface, min-bid) with a fast window (wait 1 epoch, observe 1, skip finalization) and must exit
0 — a full devnet OOMs free-tier GitHub runners, so there is no nightly CI for this; the sweep is the
gate. It deliberately excludes the negative controls (`cb-sigverify-diff-control`, which fails by
design), the ws scenarios (`cb-ws-stream*`, which need a submodule-built helix — see
[Testing the websocket header stream](#testing-the-websocket-header-stream)), and `cb-signer`.

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

The ws header-stream scenarios need a helix built from the bundled `./helix` submodule. The public
`ghcr.io/gattaca-com/helix-relay:main` image **stubs** the stream admission (it refuses the stream for every
proposer — the real logic is in gattaca's private build), so against `:main` these scenarios silently degrade
to HTTP fallback. The `develop` submodule carries the working public admission.

```bash
just build-helix-image                                  # -> local/helix-relay:kurtosis (from ./helix)
echo 'HELIX_RELAY_IMAGE=local/helix-relay:kurtosis' >> .env
just e2e configs/generated/cb-ws-stream.yml             # or any get_header=stream compose
```

Expected: `feature.ws_header_stream` PASS with zero (or one startup-race) HTTP fallbacks. Note: against the
submodule build, `relay.validator_registrations` SKIPs (the `develop` data-api query is unpopulated in this
devnet; the check confirms registration via delivery instead). Remove the `.env` override to return non-ws
scenarios to the pulled `:main` image.

---

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

For the design of the verdict model (why only tier-1 is fatal, what `inconclusive` means, how checks
are attributed to a fork) see [`ARCH.md`](ARCH.md) and [`DESIGN.md`](DESIGN.md).
