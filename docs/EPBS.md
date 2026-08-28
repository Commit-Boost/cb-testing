# ePBS (gloas) + commit-boost + keymanager sim

A one-command, reproducible harness that stands up an **ePBS (gloas) devnet** and
verifies the full **VC → keymanager builder_config → commit-boost → buildoor**
bid loop end to end. This is the regression fixture for the gloas + commit-boost +
keymanager work.

```
just epbs-test <cl-image>   # test a CL build (see "Testing a CL")
just epbs-sim               # default local/lodestar:km run
```

Prints a clear `PASS: N/N observed slots builder-built via commit-boost (buildoor)`
and exits non-zero on failure. One devnet at a time (~15G RAM).

## Testing a CL against the sim

Test a consensus client's gloas builder API end to end, no manual commit-boost build:

```bash
git clone --recurse-submodules https://github.com/Commit-Boost/cb-testing
cd cb-testing && git submodule update --init --recursive
just epbs-test chainsafe/lodestar:v1.47.0-rc.0        # or your own cl image
just epbs-test chainsafe/lodestar:v1.47.0-rc.0 mainnet  # mainnet preset
```

First run builds the CB sidecar image + `cb-km` from the pinned `commit-boost-client`
submodule (sha-tagged, `scripts/ensure-cb-artifacts.sh`; ~a few min, once per submodule
commit), then stands up the devnet. Needs `just` + Rust + Docker + Kurtosis.

**Expected PASS:** `PASS: N/16 observed slots builder-built via commit-boost (buildoor)`,
exit 0. On chain, each builder-built block carries `signed_execution_payload_bid.message.value != 0`.

**Confirmed working (2026-08):**
- `chainsafe/lodestar:v1.47.0-rc.0` - full VC → CB → buildoor flow, canonical on chain.
- prysm (OffchainLabs/prysm#17397, `builder-rest-vc`) - 17/16 builder-built; also the
  first client to pass `--assert block-submission` on the LIVE sim (see below).

**A client that is not a single image** (prysm ships a separate beacon-chain image and
validator image, and needs `--enable-builder`) cannot go through `just epbs-test`'s single
`CL_IMAGE`. Give it a scenario file under `configs/epbs/` and run that:

```bash
just epbs-test-config configs/epbs/gloas-epbs-prysm.yaml         # one client, one file
```

**Run the whole matrix** — the CB-in-loop sim across every single-client scenario, one
PASS/FAIL/SKIP row each. A client whose image is a local build that is not present is
SKIPPED (not failed), so a fresh clone still runs the ones it can pull (lodestar) and only
skips the ones needing a local build (prysm):

```bash
just epbs-cb-matrix                                              # all configs/epbs/gloas-epbs-<client>.yaml
```

(This is distinct from `just epbs-matrix`, the assertoor cross-client sweep with NO
commit-boost in the loop.)

### Gotchas when a CL "doesn't work"

- **Stale `cb-km` is THE trap** (also in AGENTS.md Known traps). An old `cb-km` populates
  `builder_pubkeys` from the relay URL; a conformant CL rejects the bid as un-allowlisted, so every
  builder bid drops and the proposer silently self-builds - it looks like the CL is broken. `just
  epbs-test` sha-pins the binary so this cannot happen; a hand-built `cb-km` must emit
  `"builder_pubkeys":[]` on `--dry-run`.
- **`--builder.selection` defaults to `executiononly`** (always self-build) on lodestar and likely
  others. The keymanager builder_config sets `maxprofit` per key, so the sim is fine, but a bare CL run
  with a builder configured self-builds until selection is set.
- **CB `/eth/v1/builder/beacon_blocks` reveal 500s are EXPECTED with buildoor.** The proposer POSTs its
  signed gloas block to CB, which fans it out to the builders; buildoor reveals the execution payload
  over P2P (native gloas transport) and does not HTTP-accept the forward, so CB returns 500
  (`NoBuilderResponse`). The block is still canonical and the default assertion counts it. This 500 is a
  *forward* outcome, not a decode failure - see `--assert block-submission` below, which turns on CB's
  `strict_block_decode` and asserts the SSZ decode (which runs *before* the forward) succeeded.
- **`min_bid` / `builder_boost_factor` are NOT the CB-vs-local levers.** A bid rejected for a stale-cb-km
  pubkey mismatch looks identical to "lost on value" - check bid *acceptance* in the beacon-node log
  first. Per-entry `min_bid` empty means accept-any; the top-level `min_bid` is the p2p floor only.

## What it runs

`scripts/run-epbs-sim.sh` drives these phases:

1. **Launch a gloas devnet** (`configs/epbs/gloas-epbs.yaml`): geth +
   lodestar CL/VC (`local/lodestar:km`, the gloas builder-api + keymanager image) +
   buildoor as the ePBS builder. `minimal` preset, 6s slots, `gloas_fork_epoch: 0`,
   64 validators with `keymanager_enabled`, one builder.
2. **Render the CB config** (`configs/epbs/cb-config.toml.tmpl`) from the live
   beacon node: the two per-run values - `genesis_time` and
   `genesis_validators_root` - are substituted; everything else (fork versions,
   the deterministic 64-key mux derived from the fixed mnemonic, the buildoor
   relay + pubkey) is static.
3. **Add commit-boost** (`commit-boost/commit-boost:km-e2e`, branch `epbs`) as the
   PBS sidecar, named `cb-epbs` (matching the advertised URL the VC will call).
   By default (`CB_LAUNCH=service`) it is a **first-class kurtosis enclave
   service**: the rendered CB config is uploaded as a files-artifact and CB is
   added with `kurtosis service add`, so it gets enclave DNS (`cb-epbs`
   resolvable by the VC, `buildoor` resolvable by CB), appears in
   `kurtosis enclave inspect`, and is torn down by `kurtosis enclave rm` with the
   rest of the enclave - no separate container to track. `CB_LAUNCH=docker` keeps
   the legacy raw `docker run` on the enclave network as a fallback.
4. **`cb-km apply`** projects the CB mux config into per-validator keymanager
   `builder_config` docs and POSTs them to the VC's keymanager API - pointing all
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

### Prerequisites (auto-provisioned)

- **CB sidecar image + `cb-km`** - built from the pinned `commit-boost-client` submodule on first run and
  sha-tagged (`commit-boost/commit-boost:km-e2e-<sha>`) by `scripts/ensure-cb-artifacts.sh`. `just epbs-test`
  / `just epbs-sim` do this automatically; nothing to build by hand. Pin a different build with `CB_IMAGE`
  (and then `CB_KM_BIN`) if you need to. Sha-tagging is the guard against the stale-`cb-km` trap.
- **CL image** - `local/lodestar:km` by default (ChainSafe nflaig builder-api gloas image); pass your own via
  `just epbs-test <cl-image>` or `CL_IMAGE=`. A released lodestar works: `chainsafe/lodestar:v1.47.0-rc.0`.

## How the keymanager calls happen (it's not kurtosis)

A common misread of this harness: kurtosis does **not** make any keymanager API
call. Kurtosis only **enables** the keymanager API on the validator client. The
`builder_config` POSTs that route the gloas bid flow through commit-boost are made
by our own tool, `cb-km apply`, from **outside** the enclave. The two halves:

**1. Kurtosis enables the keymanager API (a participant flag, nothing more).**
`configs/epbs/gloas-epbs.yaml` sets one flag on the participant:

```yaml
participants:
  - el_type: geth
    cl_type: lodestar
    vc_image: local/lodestar:km
    keymanager_enabled: true      # <-- the whole kurtosis contribution
    validator_count: 64
```

That flag makes the ethereum-package launch lodestar's VC with the keymanager
turned on and authenticated, and publish its port - 
`--keymanager --keymanager.authEnabled=true --keymanager.port=...
--keymanager.tokenFile=/keymanager/keymanager.txt`
(`ethereum-package/src/vc/lodestar.star`). The bearer token is a **well-known
static file** shipped by the package
(`static_files/keymanager/keymanager.txt`); `configs/epbs/km-token.txt` is a copy
of that same value, which is why an out-of-enclave client authenticates. Kurtosis
never calls the API; it only stands up an authenticated, reachable keymanager and
writes the token file the VC checks against.

**2. `cb-km apply` (our tool) makes the actual `builder_config` POSTs.**
Everything that writes a `builder_config` is done by `cb-km` from the host - it
could equally be `curl` or any orchestrator. `scripts/run-epbs-sim.sh`:

- discovers the keymanager **port** kurtosis published and stages the **token**
  into the overlay (`run-epbs-sim.sh` lines 111, 137, 141-143):
  ```bash
  VC_KM="$(kurtosis port print "$ENCLAVE" vc-1-geth-lodestar http-validator)"   # the km port
  cp configs/epbs/km-token.txt "$RUN_DIR/km-token.txt"                          # the bearer token
  sed -e "s|__VC_KM_URL__|$VC_KM|" -e "s|__TOKEN_PATH__|$(pwd)/$RUN_DIR/km-token.txt|" \
      configs/epbs/km-overlay.toml.tmpl > "$RUN_DIR/km-overlay.toml"
  ```
- then invokes `cb-km`, which projects the CB mux config into one `builder_config`
  doc per validator and **POSTs** each to
  `$VC_KM/eth/v1/validator/<pubkey>/builder_config` with `Authorization: Bearer
  <token>` (`run-epbs-sim.sh` line 182):
  ```bash
  "$CB_KM_BIN" apply --config "$RUN_DIR/cb-config.toml" --overlay "$RUN_DIR/km-overlay.toml"
  ```

The same authenticated `GET`/`POST` on
`/eth/v1/validator/<pubkey>/builder_config` is all the `--assert preserve` mode
uses directly via `curl` - proof that the keymanager calls are an out-of-enclave
step, not a kurtosis one.

**The loop in one line:** kurtosis boots a keymanager-enabled VC; `cb-km apply`
writes each key's `builder_config` (`url = commit-boost`, `auth_data = buildoor`);
the VC then calls commit-boost for bids per that stored config, and CB fans the
request out to buildoor.

## Assertion modes

The default run asserts the builder loop end to end (builder-built blocks via CB).
Opt-in modes (`--assert <mode>`, or `just epbs-sim-assert <mode>`) turn a merged
feature into a live regression check:

- **`p2p`**: the `min_bid` p2p floor. cb-km projects `min_bid_p2p_eth = "0.2"`
  into a key-level `min_bid` of 200000000 Gwei, above buildoor's p2p bid, while
  the CB (builder-API) entry keeps `min_bid = 0`. buildoor runs two bid channels:
  a builder-API path served to CB on request, and a **p2p-bidder** that publishes
  a competing 101000000 Gwei bid every slot to the BN's `publishExecutionPayloadBid`
  endpoint (which pools it and gossips it). The BN's `produceBlockV4` therefore
  floors that p2p bid on every builder-built slot and selects the CB bid instead.
  The mode HARD-fails unless the floor fires at least once
  (`Ignoring p2p bid below min bid slot=.. bidValue=101000000 minBid=200000000`),
  the selected `bidSource` is the CB URL on every selection, and no p2p bid ever
  wins. A floored p2p bid is nulled *before* candidate ranking, so it never
  appears in `Ranked builder bid candidates`; the rejection line is the floor's
  only signal, and the asserts read the **full** BN log (`kurtosis service logs -a`)
  because the once-per-slot rejection line does not survive the default 200-line
  tail.
- **`preserve`**: `cb-km apply --preserve-entries` keeps a third-party
  `builder_config` entry that a plain apply drops. Keymanager-API only; skips the
  builder loop.
- **`block-submission`**: CB SSZ-decodes the revealed gloas `SignedBeaconBlock` at
  `POST /eth/v1/builder/beacon_blocks`. The mode turns on CB's `strict_block_decode`
  (default OFF = blind pipe, forwards the body unparsed), so CB parses the SSZ body
  itself. Decode runs *before* the fan-out, so the status codes split cleanly:
  **4xx = decode failed** (an SSZ over-read / `OffsetOutOfBounds`, or `NotGloasBlock`);
  **5xx = decoded, then no builder HTTP-accepted the forward** (`NoBuilderResponse`,
  EXPECTED with buildoor, which reveals over p2p); **2xx = decoded + forwarded**. PASS
  requires `strict_block_decode` on, ≥1 block past the decode, and zero decode-4xx /
  `OffsetOutOfBounds` (forward 5xx are reported, not failed). Defaults `PRESET=mainnet`
  because CB is compiled `MainnetEthSpec` - a minimal-preset block over-reads and 400s.
  **The block must also come from a client that finalizes on mainnet** so buildoor
  activates and the proposer reaches builder-built (reveal-worthy) slots: prysm does
  (verified, 17 blocks decoded, 0 4xx); the lodestar harness image does not
  (`gloas-epbs.yaml` comment), so for lodestar the decode is verified out-of-band.
- Also: **`builder-down`** (buildoor stop never stalls the proposer) and
  **`request-auth`** (`verify_builder_request_auth` ON, zero `AuthSigVerify`).

## Known caveats

- **Commit-boost forwards ePBS bids without verifying their signatures.** The
  beacon node verifies the bid's `builder_index` against the on-chain builder
  registry and collateral, so the CB bid path is a blind pipe by design.

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
`commit-boost` **sidecar launcher** - a fork-only feature. Upstream
`ethpandaops/ethereum-package` has **no** `mev_resolver.star` and no commit-boost
sidecar at all. So the submodule cannot simply be pointed at upstream: that would
delete the sidecar wiring the rest of the suite runs on.

**The fork is ~110 upstream PRs behind on gloas.** Pinned submodule
`1b255a4` (branch `cb-testing`) sits on upstream base `8a11379` (ethpandaops
merge point, PR #1366 era). The gloas-genesis + EIP-8282 + lifecycle fixes that
`local/lodestar:km` needs landed upstream around `0350d2e9` ("Enable buildoor for
Nimbus", #1476, 2026-08-13) - the ref this harness launches. Concretely, the
pinned fork:
  - rejects `network_params.deploy_eip8282_contracts` and
    `buildoor_params.lifecycle` (not in its `sanity_check.star` /
    `input_parser.star` schema - only `run_lifecycle_test` and `epbs_builder`
    exist there);
  - bakes a `genesis.ssz` the gloas image cannot deserialize
    (`progressiveContainer` offset mismatch).

**Bringing gloas into the fork is not a clean one-session op.** The fork's
CB-specific commits (custom mev_type `7efe6fe`, the commit-boost sidecar launcher,
the signer container, helix N-relay, subsidies) are **interleaved with periodic
`upstream/main` merges**, not a tidy patch series - so neither "merge upstream
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
through CB is exactly what `cb-km apply` does - a step the package launcher does
not perform. So "native, no `cb-km apply`" requires teaching an ethereum-package
sidecar launcher to project the gloas `builder_config`, which is an upstream
(fork) package change and out of scope for this harness.

**Net:** this harness keeps the upstream `EP_PACKAGE` + first-class CB service +
`cb-km apply`. Remaining follow-ups (separate tickets): (1) integrate into the
typed `sim` scenario generator; (2) bump the `Commit-Boost/ethereum-package`
submodule to a gloas-capable base **and** add an epbs-aware commit-boost sidecar
launcher that writes the gloas `builder_config`, then wire a real `epbs`/`custom`
mev_type - retiring the manual CB add + `cb-km apply`; (3) remove
`skip_sigverify` once progressive-SSZ hashing lands.

- **This is the `epbs`-branch harness**, not yet the typed `sim` generator - see
  follow-up (1) above.
