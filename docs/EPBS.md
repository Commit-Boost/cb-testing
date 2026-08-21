# ePBS (gloas) + commit-boost + keymanager sim

A one-command, reproducible harness that stands up an **ePBS (gloas) devnet** and
verifies the full **VC → keymanager builder_config → commit-boost → buildoor**
bid loop end to end. This is the regression fixture for the gloas + commit-boost +
keymanager work.

```
just epbs-sim          # or: ./scripts/run-epbs-sim.sh
```

Prints a clear `PASS: N/N observed slots builder-built via commit-boost (buildoor)`
and exits non-zero on failure. One devnet at a time (~15G RAM).

## What it runs

`scripts/run-epbs-sim.sh` drives these phases:

1. **Launch a gloas devnet** (`configs/epbs/gloas-epbs.yaml`): geth +
   lodestar CL/VC (`local/lodestar:km`, the gloas builder-api + keymanager image) +
   buildoor as the ePBS builder. `minimal` preset, 6s slots, `gloas_fork_epoch: 0`,
   64 validators with `keymanager_enabled`, one builder.
2. **Render the CB config** (`configs/epbs/cb-config.toml.tmpl`) from the live
   beacon node: the two per-run values — `genesis_time` and
   `genesis_validators_root` — are substituted; everything else (fork versions,
   the deterministic 64-key mux derived from the fixed mnemonic, the buildoor
   relay + pubkey) is static.
3. **Insert commit-boost** (`commit-boost/commit-boost:km-e2e`, branch `epbs`) as
   a PBS sidecar container on the enclave's docker network, named `cb-epbs`
   (matching the advertised URL the VC will call).
4. **`cb-km apply`** projects the CB mux config into per-validator keymanager
   `builder_config` docs and POSTs them to the VC's keymanager API — pointing all
   64 validators' builder URL at commit-boost with `auth_data = buildoor`.
5. **Wait for buildoor activation.** buildoor submits a builder *deposit* to the
   EIP-8282 registry on boot and only bids once that deposit is included and
   **activated** (an activation-queue delay, empirically ~epoch 4 / slot ~33 on
   the minimal preset). Until then commit-boost gets `204 No Content`
   ("builder not active on chain"). The script polls CB's log for buildoor's
   first bid before it starts measuring.
6. **Observe + assert** over a window of slots:
   - `BN → CB` bid calls (CB serves `/eth/v1/builder/execution_payload_bid/...`),
   - buildoor bids via CB (`auction winner relay_id="buildoor-mux"`, `version=Gloas`),
   - **builder-built blocks on chain**: each gloas block carries a
     `signed_execution_payload_bid`; a builder-built block has
     `message.value != 0` with `builder_index = 0` (buildoor). Self-built blocks
     have `value = 0`.

   PASS requires `>= MIN_BUILDER_SLOTS` builder-built slots in the window (plus at
   least one auction win and one bid call).

### Knobs (env vars)

| var | default | meaning |
|-----|---------|---------|
| `ENCLAVE` | `epbs-sim` | kurtosis enclave name |
| `OBSERVE_SLOTS` | `16` | slots to watch once buildoor is active |
| `MIN_BUILDER_SLOTS` | `8` | PASS threshold (allows some missed slots) |
| `BUILDOOR_ACTIVATION_TIMEOUT` | `600` | seconds to wait for the builder deposit to activate |
| `KEEP` | `0` | `1` leaves the enclave + `cb-epbs` running for inspection |
| `CB_IMAGE` | `commit-boost/commit-boost:km-e2e` | the CB sidecar image |
| `CB_KM_BIN` | auto | path to the `cb-km` binary |
| `EP_PACKAGE` | `github.com/ethpandaops/ethereum-package` | ethereum-package to launch |

### Prerequisites (local images / binary)

- `local/lodestar:km` — ChainSafe nflaig builder-api gloas image (gloas builder
  API + keymanager `builder_config`).
- `commit-boost/commit-boost:km-e2e` — CB with the ePBS bid pipe + km-tool (branch `epbs`).
- `cb-km` — the mux → keymanager projector (`cargo build -p cb-km-tool --release`;
  the script auto-discovers it on `PATH` or a known worktree, else set `CB_KM_BIN`).

## Known caveats

- **Bid signature verification is skipped** (`skip_sigverify = true` in the CB
  config, with the same explanatory comment). The gloas signing-domain overrides
  in the config are correct, but the devnet stack hashes gloas containers as
  EIP-7495 progressive containers (+ EIP-7916 progressive blob list) while CB's
  pinned lighthouse (v8.2.2) hashes classic SSZ containers, so the computed
  signing roots differ. Sigverify stays off until the progressive-SSZ hashing is
  upgraded (ticket exists).

- **Uses UPSTREAM `github.com/ethpandaops/ethereum-package`, not this repo's
  pinned `ethereum-package` submodule.** The pinned submodule predates
  gloas-genesis compatibility with `local/lodestar:km`: it bakes a `genesis.ssz`
  the gloas image cannot deserialize (`progressiveContainer` offset mismatch) and
  its MEV resolver still wires a mev-boost sidecar for `mev_type: buildoor`.
  Kurtosis fetches + caches the upstream package; pin it with
  `EP_PACKAGE=github.com/ethpandaops/ethereum-package@<ref>` for full determinism.

- **This is the `epbs`-branch SCRATCH harness** — a hand-wired manual CB-insert,
  not the typed `sim` generator. It is intentionally unopinionated: a working loop
  + assertion that stands up regression-test infra now. Follow-ups (separate
  tickets): (1) integrate into the typed `sim` scenario generator; (2) upgrade the
  repo's `ethereum-package` submodule and wire a real `epbs` mev_type that launches
  commit-boost as the sidecar (today the fork's `epbs` mev_type resolves
  `sidecar=none`), so no manual `docker run` / `cb-km apply` is needed;
  (3) remove `skip_sigverify` once progressive-SSZ hashing lands.
