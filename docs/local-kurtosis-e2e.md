# Local Kurtosis e2e for commit-boost (PBS / ePBS) — setup runbook

> Living doc. Captures the *actual* working steps to build a local commit-boost image,
> deploy a Kurtosis devnet, run a PBS simulation, and verify it end-to-end on this box.
> Written as we prove each step, so it reflects reality, not theory.
> Box: Linux, Docker 29.6, 32 cores / 60 GB. Status: **WORKING — cb-basic PASSES** (2026-07-30).
>
> First green e2e run: v0.11.0 SSZ commit-boost image (`commit-boost/commit-boost:kurtosis` from `main`),
> kurtosis 1.18.1, cb-basic. cb-verify overall = PASS (13 PASS / 1 SKIP finality / 1 WARN relay-latency
> p95 914ms on a loaded box / 0 FAIL): 33 payloads delivered + 33/33 hash-matched on-chain, 62 get_header
> bids, 62 submit_blinded_block 200s, 100% MEV delivery, 128/128 validators registered, 0/32 missed slots.
> Getting here required the kurtosis 1.18.1 pin + 3 latest-helix fixes (network_config, cores, pre-genesis
> wait) — all below.

## The two flows (pick one)
- **cb-testing (this repo) — the verification harness.** `cb-verify` Rust binary with tiered
  pass/fail checks, 6 scenarios, forked `JasonVranek/ethereum-package` submodule, helix relay.
  This is the authoritative flow (success criteria baked in). **We use this.**
- **commit-boost-client `just kurtosis-*`** — lighter alt: upstream `ethereum-package`,
  `mev_type: commit-boost`, mev-boost-relay, no pass/fail harness. Good for a quick smoke.

## Prerequisites (this box)
- Docker + buildx: PRESENT (29.6).
- Rust 1.91 (cb-testing pins `edition 2024`, `rust-version 1.91`): repo toolchain present.
- Kurtosis CLI >= 0.90: **TODO install** (see below).

## Step 0 — Install Kurtosis CLI  (STATUS: pending approval)
Documented + CI-proven recipe (from cb-testing `.github/workflows/integration.yml`):
```bash
echo "deb [trusted=yes] https://sdk.kurtosis.com/kurtosis-cli-release-artifacts/ /" \
  | sudo tee /etc/apt/sources.list.d/kurtosis.list
sudo apt update && sudo apt install -y kurtosis-cli
kurtosis analytics disable
kurtosis version   # verify; record the version here: ____
```

## Step 1 — Build the commit-boost image from `main`  (STATUS: done, CONFIRMED)
`main` = the v0.11.0 SSZ content (#467/#468/#481/#482 SSZ + #465 Stader + #480 Dirk), tip `635384f`.
```bash
cd /home/j/code/commit-boost-client && git checkout main && git pull --ff-only origin main
just build-all kurtosis        # -> image commit-boost/commit-boost:kurtosis  (crate=commit-boost)
```
Image name is `commit-boost/commit-boost:<tag>` (NOT `pbs:*`; the `.env.example` confirms
`commit-boost/commit-boost:latest` is the default). Long first build (full Rust release compile in
docker buildx). Verify: `docker image inspect commit-boost/commit-boost:kurtosis`.

## Step 2 — Prepare cb-testing  (STATUS: done, CONFIRMED)
```bash
cd /home/j/code/cb-testing
git submodule update --init --recursive        # pull forked ethereum-package @ 4844f884 (was EMPTY)
# .env — override BOTH images (see gotcha):
#   MEV_BOOST_IMAGE=commit-boost/commit-boost:kurtosis     (local CB build)
#   HELIX_RELAY_IMAGE=ghcr.io/gattaca-com/helix-relay:main (public; we don't build helix)
just generate-configs                           # -> configs/generated/*.yml
```
**GOTCHA (confirmed):** the generator defaults `helix_relay_image` to the LOCAL tag
`helix-relay:kurtosis`, which does not exist unless you build helix (helix lives in the ws-workspace
meta-repo, not cb-testing). For a PBS-only run, override `HELIX_RELAY_IMAGE` to the public
`ghcr.io/gattaca-com/helix-relay:main` in `.env`. Confirmed cb-basic then references only the local
CB image + public helix/reth-rbuilder/lighthouse.
Pre-pull the public images so the run doesn't stall:
`docker pull ghcr.io/gattaca-com/helix-relay:main; docker pull ethpandaops/reth-rbuilder:develop; docker pull sigp/lighthouse:latest`

cb-testing state: NOT stale-broken — J confirmed it's version-independent and in use; treat
sim failures as real signal. (Refactoring/improving cb-testing is a separate parallel subtask.)

## Helix pre-genesis panic (3rd helix fix — CONFIRMED + fixed)
After the config fixes, latest `:main` helix still crashed at runtime:
`panicked at chain_info.rs: called Option::unwrap() on a None value` in `ChainInfo::current_slot`
(from `HousekeeperTile::new` at boot). ROOT CAUSE: helix eagerly computes `current_slot()` at startup,
which is `None` before genesis. Confirmed timing: helix started 19s BEFORE genesis (devnet
`genesis_delay = 20`). FIX: wrap helix's launch to wait for genesis in
`ethereum-package/src/mev/helix/helix_relay_launcher.star` — add `"GENESIS_TIME": str(genesis_timestamp)`
to the service `env_vars`, and set `entrypoint=["sh","-c"]` +
`cmd=['until [ "$(date +%s)" -ge "$GENESIS_TIME" ]; do sleep 1; done; exec /app/helix-relay --config <path>']`.
(sh+date are in the image; the ~20s wait fits kurtosis's 60s port-check.) RESULT: full stack deploys —
helix-relay RUNNING, commit-boost RUNNING, dora/spamoor/prometheus up; the run reaches `cb-verify`.

## Step 3 — Run the sim  (STATUS: full stack deploys; cb-verify runs)
```bash
just testnet configs/generated/cb-basic.yml
# = run-and-verify.sh: rm stale enclave -> kurtosis run CB-Testnet -> cb-verify
```

## Step 4 — Verify (the pass bar)  (STATUS: pending)
cb-verify tiers (exit 0 = all pass):
- Tier 1 (must): chain_finality, sync_status, cb_running, relay.payloads_delivered_multi,
  payload_hash_match, mux.routing.
- Tier 2 (should): missed_slots <10%, builder_blocks_received >0, mev_delivery_rate >=30%,
  validator_registrations =100%.
- Tier 3: CB Prometheus `cb_pbs_relay_status_code_total{endpoint=get_header|submit_blinded_block|...}`.
Monitor live: dora block explorer (additional_service), `just show-logs CB-Testnet`,
`kurtosis enclave inspect CB-Testnet`, `just kurtosis-logs <service>`.

## Cleanup
```bash
kurtosis enclave rm -f CB-Testnet     # single enclave
kurtosis clean -a                     # full wipe
```

## Gotchas (fill in as hit)
- **helix `:main` drift → grpc invalid-UTF8 at `Adding service 'helix-relay'` (CONFIRMED blocker).**
  Symptom: `kurtosis run` aborts at the helix-relay service add with
  `grpc: error while marshaling: string field contains invalid UTF-8`, half-building the enclave (no
  commit-boost / helix-relay-api). ROOT CAUSE (via `docker logs` on the exited helix container, NOT the
  kurtosis error): helix panics on startup — `config.rs: failed to parse config file:
  network_config: untagged and internally tagged enums do not support enum input`. The public
  `ghcr.io/gattaca-com/helix-relay:main` (moving tag) drifted and its config schema no longer matches the
  `helix_relay_config` the pinned fork (`ethereum-package @ 4844f884`) renders. The UTF-8 grpc error is a
  SECONDARY symptom of kurtosis streaming the crashing container. Reproduced on kurtosis 1.20.0 AND 1.18.1
  (NOT a kurtosis-version issue). FIX (chosen: track latest helix): reconcile the embedded
  `helix_relay_config` in `scripts/generate_kurtosis_configs.py` to current `:main`. Source of truth =
  the `:main` BINARY's serde metadata (`docker create` + `docker cp /app/helix-relay` + `strings`), NOT
  the ws-workspace helix checkout (a divergent WS branch, unreliable). Two drifts fixed:
    1. Deleted `network_config: !Custom {dir_path, genesis_validator_root, genesis_time}` — `:main` removed
       it; the relay now fetches chain spec + genesis from the beacon node at startup.
    2. `cores:` (CoresConfig) is now 10 fields: dropped `sub_workers`; added `decoder: [0]` (Vec<usize>) +
       `simulator`/`top_bid`/`data_gatherer`/`block_merging`/`housekeeper` (usize `0`). The panic
       mis-reported this as top-level `missing field \`decoder\`` (a `#[serde(flatten)]` artifact).
  FAST iteration loop (seconds, not 10-min kurtosis runs): `docker run --rm -v cfg:/app/config.yaml
  <helix:main>` panics ~1s per bad field; success = it reaches the beacon-fetch stage past config parse.
  PIN vs LATEST: pin via `HELIX_RELAY_IMAGE=<tag/digest>` in `.env` (no template change). Template shape is
  COUPLED to helix version, so a pinned OLD helix needs the OLD template. For both (relevant to the
  WebSocket-vs-helix work), add a schema switch in the generator keyed off the image tag. Core VALUES (all
  core 0) are a smoke-test choice, not a realistic perf layout.
- **kurtosis version + config-version clash:** J runs 1.18.1. A newer CLI (1.20.0) writes
  `~/.config/kurtosis/kurtosis-config.yml` at `config-version: 9`, which 1.18.1 can't read
  (`ConfigVersion(9) ... newer than ConfigVersion_v7`). Fix on downgrade: `rm` that file (engine restart
  regenerates it), then `kurtosis analytics disable`. (This box: pinned to 1.18.1.)
- Forked ethereum-package submodule is load-bearing + must be `--init`ed (empty otherwise).
- `--image-download always` still uses purely-local tags if they aren't registry refs.
- Image-tag mismatch across generator / example config / justfile (see Step 1).
- (add machine-specific / version-pin issues here)

## What ePBS e2e would additionally need (scaffold — not this run)
- ePBS-aware relay + builder (stock helix/reth-rbuilder speak legacy PBS, not
  getExecutionPayloadBid / in-protocol bids).
- A gloas-capable ethereum-package (fork bump).
- A CB image built from the `epbs` branch.
- cb-verify: add an endpoint arm to the `cb_metrics.rs` status-code matrix for the ePBS
  endpoints, and a beacon-side check analogous to `payload_matching` for the envelope flow.
