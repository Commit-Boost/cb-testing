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
3. **Add commit-boost** (`commit-boost/commit-boost:km-e2e`, branch `epbs`) as the
   PBS sidecar, named `cb-epbs` (matching the advertised URL the VC will call).
   By default (`CB_LAUNCH=service`) it is a **first-class kurtosis enclave
   service**: the rendered CB config is uploaded as a files-artifact and CB is
   added with `kurtosis service add`, so it gets enclave DNS (`cb-epbs`
   resolvable by the VC, `buildoor` resolvable by CB), appears in
   `kurtosis enclave inspect`, and is torn down by `kurtosis enclave rm` with the
   rest of the enclave — no separate container to track. `CB_LAUNCH=docker` keeps
   the legacy raw `docker run` on the enclave network as a fallback.
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
| `CB_LAUNCH` | `service` | `service` = CB as a first-class enclave service; `docker` = legacy raw `docker run` |
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
  pinned `ethereum-package` submodule** (`EP_PACKAGE`). See the section below for
  exactly why the submodule cannot be bumped or swapped cleanly today.

## Native ePBS mev_type: investigation & submodule-upgrade blockers

The end goal is a native `ethereum-package` `epbs` (or `buildoor`) mev_type that
launches commit-boost as the sidecar, so this harness needs no manual CB add and
no `cb-km apply`. Investigation (2026-08) found three coupled blockers; the ceiling
reachable without an **upstream ethereum-package change** is the first-class-service
harness above, not a native mev_type.

**The submodule is the `Commit-Boost/ethereum-package` FORK, on purpose.** The
repo's ~10 non-epbs scenarios (`cb-basic`, `cb-mux`, …) depend on the fork's
decomposed MEV resolver (`src/package_io/mev_resolver.star`) and its
`commit-boost` **sidecar launcher** — a fork-only feature. Upstream
`ethpandaops/ethereum-package` has **no** `mev_resolver.star` and no commit-boost
sidecar at all. So the submodule cannot simply be pointed at upstream: that would
delete the sidecar wiring the rest of the suite runs on.

**The fork is ~110 upstream PRs behind on gloas.** Pinned submodule
`1b255a4` (branch `cb-testing`) sits on upstream base `8a11379` (ethpandaops
merge point, PR #1366 era). The gloas-genesis + EIP-8282 + lifecycle fixes that
`local/lodestar:km` needs landed upstream around `0350d2e9` ("Enable buildoor for
Nimbus", #1476, 2026-08-13) — the ref this harness launches. Concretely, the
pinned fork:
  - rejects `network_params.deploy_eip8282_contracts` and
    `buildoor_params.lifecycle` (not in its `sanity_check.star` /
    `input_parser.star` schema — only `run_lifecycle_test` and `epbs_builder`
    exist there);
  - bakes a `genesis.ssz` the gloas image cannot deserialize
    (`progressiveContainer` offset mismatch).

**Bringing gloas into the fork is not a clean one-session op.** The fork's
CB-specific commits (custom mev_type `7efe6fe`, the commit-boost sidecar launcher,
the signer container, helix N-relay, subsidies) are **interleaved with periodic
`upstream/main` merges**, not a tidy patch series — so neither "merge upstream
`0350d2e9` into `cb-testing`" (~110 PRs, conflicts concentrated in
`mev/` + `package_io/`, and it revalidates the whole suite on a memory-tight box)
nor "cherry-pick the CB feature onto upstream" is low-risk. This belongs in its
own ticket against the fork, validated across all scenarios.

**Even with a bumped fork, `epbs`-mev-type-with-CB needs an upstream change.**
The fork's `epbs` mev_type resolves `sidecar=none` by design, and its
`custom` mev_type with `mev_sidecar=commit-boost` would launch the **classic**
(pre-gloas) commit-boost sidecar: its launcher writes the traditional VC
`--builder` endpoint and has **no** notion of the gloas keymanager `builder_config`
(the per-validator doc with `auth_data=buildoor`). Routing the gloas bid flow
through CB is exactly what `cb-km apply` does — a step the package launcher does
not perform. So "native, no `cb-km apply`" requires teaching an ethereum-package
sidecar launcher to project the gloas `builder_config`, which is an upstream
(fork) package change and out of scope for this harness.

**Net:** this harness keeps the upstream `EP_PACKAGE` + first-class CB service +
`cb-km apply`. Remaining follow-ups (separate tickets): (1) integrate into the
typed `sim` scenario generator; (2) bump the `Commit-Boost/ethereum-package`
submodule to a gloas-capable base **and** add an epbs-aware commit-boost sidecar
launcher that writes the gloas `builder_config`, then wire a real `epbs`/`custom`
mev_type — retiring the manual CB add + `cb-km apply`; (3) remove
`skip_sigverify` once progressive-SSZ hashing lands.

- **This is the `epbs`-branch harness**, not yet the typed `sim` generator — see
  follow-up (1) above.
