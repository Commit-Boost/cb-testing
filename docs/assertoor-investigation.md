# Assertoor for cb-testing: investigation and integration proposal

Should ethpandaops' **assertoor** be added to cb-testing, and if so how? This
report grounds the answer in the existing bespoke harness
(`scripts/run-epbs-sim.sh`), the three gloas args files under `configs/epbs/`,
the assertoor source and playbooks, and the already-drafted
`configs/epbs/gloas-epbs-matrix.yaml`.

**Bottom line up front.** Adopt assertoor as a **complement, not a replacement**.

- **Tier 1 (do now, cheap, high value):** ship `gloas-epbs-matrix.yaml` as a
  standing cross-client baseline that runs the ethpandaops gloas-dev playbooks
  against lodestar / lighthouse / teku / nimbus / grandine. This buys multi-CL
  regression coverage of the generic gloas builder mechanism for near-zero
  maintenance. **Caveat that must be stated loudly: this path does NOT test
  commit-boost.** It is VC to buildoor direct.
- **Tier 2 (worth a spike, partial):** a custom assertoor playbook can cover the
  *HTTP-observable* half of the CB-in-loop path (get_header 204/200, block-count
  on chain) via `check_http_json` / `run_shell`, but it **cannot** cover the half
  our `--assert` modes actually lean on, because those grep commit-boost and
  beacon-node **container logs** and assertoor has no log access.
- **Tier 3:** keep `run-epbs-sim.sh` as the authoritative CB-in-loop regression
  gate. Assertoor sits alongside it as the cross-client and chain-health layer.

---

## 1. What we have today (the baseline to complement)

`scripts/run-epbs-sim.sh` stands up a gloas devnet (`configs/epbs/gloas-epbs.yaml`),
inserts commit-boost as a first-class kurtosis service (`cb-epbs`), runs
`cb-km apply` to point every validator's keymanager `builder_config` at
commit-boost, and then asserts the loop
**VC -> keymanager builder_config -> commit-boost -> buildoor**. It discovers
services by fixed name (`cl-1-lodestar-geth` for the BN,
`vc-1-geth-lodestar` for the keymanager VC; `run-epbs-sim.sh` lines 469-471) and
its assertions fall into two families:

**A. On-chain observation (HTTP against the beacon API).** `classify_blocks`
and the default-mode counter read `/eth/v2/beacon/blocks/{slot}` and classify a
gloas block as builder-built when `signed_execution_payload_bid.message.value != 0`
(lines 228-245, 609-625). Head-slot polling reads
`/eth/v1/beacon/headers/head`. These are pure HTTP-JSON reads.

**B. Log grepping (the load-bearing half).** Every `--assert` mode ultimately
greps container logs pulled with `kurtosis service logs -a`:

| `--assert` mode | Core signal | Source |
|---|---|---|
| default | `auction winner ... buildoor-mux`; `execution_payload_bid` bid calls | CB log (`cb_logs`, line 139) |
| `p2p` | `Ignoring p2p bid below min bid ... minBid=200000000`; `Selected builder block ... bidSource=...cb-epbs` vs `...p2p` | **BN log** (`bn_logs`, line 153) |
| `block-submission` | `Responded with <code> ... method=/eth/v1/builder/beacon_blocks`; `OffsetOutOfBounds` | CB access log + BN log |
| `builder-down` | `no header available for slot` (204); `get_execution_payload_bid failed ... Internal` (500) | CB log |
| `request-auth` | `auth signature verification failed` / `AuthSigVerify` (401) | CB log |
| `preserve` | keymanager API GET/POST of `builder_config` (no logs) | VC keymanager API |

The comments on `cb_logs` / `bn_logs` are explicit that `-a` (full history) is
load-bearing: the once-per-slot bid-selection lines do not survive the default
200-line log tail (lines 138-152). **This log-grep dependency is the single most
important fact for the assertoor question** (see section 4).

The three args files:

- `gloas-epbs.yaml` — single lodestar (`local/lodestar:km`), minimal preset,
  `gloas_fork_epoch: 0`, 64 validators, `keymanager_enabled: true`,
  `mev_type: buildoor`. The CB-in-loop default fixture.
- `gloas-epbs-2cl.yaml` — adds a lighthouse participant purely to carry
  finalization on the mainnet preset (the lodestar km image packs only ~1
  attestation aggregate per block; see the file header). lighthouse has no gloas
  builder API, so its slots self-build; only lodestar routes through CB.
- `gloas-epbs-matrix.yaml` — the already-drafted cross-client matrix that pulls
  in assertoor. This is Tier 1 (section 6).

---

## 2. Assertoor fundamentals

Assertoor (github.com/ethpandaops/assertoor) is ethpandaops' declarative
Ethereum-network test runner. Grounding, from the repo README and source:

- **Model.** A *playbook* is a YAML test = an ordered list of *tasks*. Tasks can
  run sequentially, concurrently (`run_tasks_concurrent`), or as a matrix
  (`run_task_matrix`). Tasks pass data through named outputs
  (`tasks.<id>.outputs.<field>`), and jq-style expressions (`"| ..."`) compute
  config values inline. Tasks may be inlined in the config or **sideloaded from
  external URLs**.
- **How it runs in a kurtosis devnet.** Add it under `additional_services:
  [assertoor]` in the ethereum-package args, and pass `assertoor_params.tests`
  as a list of `{ file: <url-or-path> }` entries. This is exactly what
  `gloas-epbs-matrix.yaml` lines 54-62 already do. Assertoor auto-discovers the
  enclave's CL/EL endpoints, so playbooks do not hardcode beacon URLs (the
  gloas-dev playbooks never mention one).
- **Reporting.** Three channels: a **web UI/dashboard** (real-time test/task
  status, logs, results — exposed as an enclave service, alongside `dora` in the
  matrix config), an **HTTP API** for programmatic status, and a process **exit
  code** for CI. Per-task pass/fail rolls up to a per-playbook verdict.
- **Task catalog.** 53 task types (`pkg/tasks/*/task.go`). The ones relevant to
  us are in the appendix. The important families: `check_consensus_*` and
  `check_execution_*` (chain-health asserts), `generate_*` (deposits, exits,
  builder deposits/exits, transactions), `get_*` (specs, pubkeys, mnemonics),
  `check_http_json` / `check_eth_call` (assert arbitrary HTTP/EL endpoints), and
  the escape hatches `run_shell`, `run_command`, `run_javascript`,
  `run_external_tasks`.

---

## 3. The gloas-dev playbooks (what they actually assert)

Full directory listing of
`github.com/ethpandaops/assertoor/tree/master/playbooks/gloas-dev` (15 files):
`_header.yaml`, `builder-deposit.yaml`, `builder-deposit-spam.yaml`,
`builder-lifecycle.yaml`, `builder-prefork-onboard.yaml`,
`builder-prefork-queuefill.yaml`, `deploy-eip8282-contracts.yaml`,
`exit-builders.yaml`, `exit-conflict.yaml`, `prefork-queue-fill.yaml`,
`prefork-queue-fill-public.yaml`, `slash-active-validators.yaml`,
`slash-validators.yaml`, `slashing-exit-conflict.yaml`, `worst-case-block.yaml`.

The `_header.yaml` group description: "GLOAS-fork tests focused on the ePBS
payload-publishing model and the new builder role (validators with 0xB0
withdrawal credentials)."

### 3.1 builder-deposit.yaml — the minimal smoke

Four tasks (from the raw YAML):

1. `check_clients_are_healthy` (minClientCount 1).
2. `generate_child_wallet` — a funded child wallet (prefund 10 ETH).
3. `get_random_mnemonic` — fresh builder key material.
4. `generate_builder_deposits` — submits ONE builder deposit to the EIP-8282
   builder deposit system contract (`0x0000bFF4...8282`) as raw calldata signed
   under `DOMAIN_BUILDER_DEPOSIT`, `awaitReceipt` + `awaitInclusion`.

Asserts: a single builder deposit is accepted on the EL and included as a
builder-deposit request on the CL. This is the "does the builder deposit path
work at all" smoke.

### 3.2 builder-lifecycle.yaml — the end-to-end builder lifecycle

The substantive one. It exercises BOTH onboarding paths and the full lifecycle:

1. **Health + specs + current slot** — `check_clients_are_healthy`,
   `get_consensus_specs` (reads `GLOAS_FORK_EPOCH`, `SLOTS_PER_EPOCH`,
   `MIN_BUILDER_WITHDRAWABILITY_DELAY`), `check_consensus_slot_range`.
2. **Timing computation** — a `run_shell` (`calc_slots`) derives, from fork epoch
   and current slot, whether the test started pre-fork or post-fork, and the
   pre-fork deposit slot / post-fork deposit slot / activation-wait epoch. Uses
   `::set-output-json` to publish computed values.
3. **Key material** — `generate_child_wallet`, `get_random_mnemonic`,
   `get_pubkeys_from_mnemonic` (count = builderCount * 3).
4. **Pre-fork onboarding (only if started pre-fork)** — waits for the pre-fork
   slot, then `generate_deposits` via the STANDARD validator deposit contract
   with `0xB0` (builder) withdrawal credentials. These sit in the pending-deposit
   queue and are converted to builders at the fork by
   `onboard_builders_from_pending_deposits`; signatures verify under the regular
   `DOMAIN_DEPOSIT`.
5. **Post-fork deposits** — two `generate_builder_deposits` batches
   (builderCount each) via the EIP-8282 builder contract.
6. **Activation** — waits to the computed active epoch, then `run_task_matrix`
   over all builder pubkeys running `check_consensus_builder_status`
   (`expectActive: true`, `failOnCheckMiss: true`). This is the assertion that
   the builders became active builders on chain.
7. **Exit** — exits the builders in two halves an epoch apart via
   `generate_builder_exits` (EIP-8282 builder exit contract), then a matrix of
   `check_consensus_builder_status` with `expectExiting: true` to confirm they
   entered the exiting state.
8. **Index reuse (only when `MIN_BUILDER_WITHDRAWABILITY_DELAY < 10`)** — waits
   for full withdrawal (balance 0), re-deposits a fresh set, and asserts they
   reuse the freed builder indices. Skipped on the default 8192-epoch delay.

Asserts, in one sentence: the full EIP-8282 builder lifecycle — deposit (both the
pre-fork 0xB0-dequeue path and the post-fork builder-contract path) -> registry
activation -> active status on chain -> builder exit -> exiting status -> (opt)
index reuse — holds across whatever CLs are in the enclave. What it does NOT
touch: it never asserts that a *builder-built block* was produced and followed;
it asserts builder *registry state*, not the bid/reveal payload flow. It also
never mentions commit-boost.

### 3.3 The other 13 (one-liners, from their descriptions)

- `builder-deposit-spam` — 1000 builder deposits with an in-flight cap; asserts
  the chain still finalizes under the deposit storm.
- `builder-prefork-onboard` — focused regression on the pre-fork
  `onboard_builders_from_pending_deposits` path only.
- `builder-prefork-queuefill` — tests whether a builder can be active in the very
  first gloas epoch (slot 0) via the deposit-queue-position trick.
- `deploy-eip8282-contracts` — deploys the EIP-8282 builder deposit/exit system
  contracts pre-fork (for devnets that omit them from genesis).
- `exit-builders` / `prefork-queue-fill(-public)` — companion deposit/exit runs
  seeded from fixed mnemonics.
- `exit-conflict`, `slashing-exit-conflict` — `process_previous_payload` /
  `process_operations` ordering regressions (EL-triggered vs voluntary exit;
  slashing vs voluntary exit).
- `slash-active-validators` / `slash-validators` — conditional slashing of
  active keys.
- `worst-case-block` — floods every operation type to its per-block limit
  simultaneously (the consensus-specs #5436 worst-case block).

These are all valuable *devnet health / spec-conformance* coverage, and all are
CL-agnostic. None of them exercise commit-boost or a PBS relay.

---

## 4. Can assertoor test the commit-boost-in-loop path?

**Partially. The HTTP-observable half yes; the log-observable half no.**

**What CAN be done natively.** `check_http_json` (config in
`pkg/tasks/check_http_json/task.go`) is far more capable than its name suggests:
it takes an arbitrary `url`, `method` (GET/POST/PUT/PATCH/DELETE/HEAD),
`headers`, `body`/`bodyRaw`, `expectStatus`/`expectStatuses`, `pollInterval`,
and a list of JSON `assertions`. Because assertoor runs inside the enclave, a
task can address `http://cb-epbs:18550` by service DNS. So we CAN, natively:

- assert commit-boost's PBS status endpoint responds;
- POST to a CB endpoint and assert a status code / JSON field;
- assert on-chain builder-built block counts by reading
  `/eth/v2/beacon/blocks/{slot}` and checking
  `signed_execution_payload_bid.message.value` (the exact `check` our default
  mode does, portable to `check_http_json` assertions or a `run_shell`).

`run_shell` (used throughout `assertoor/cb-mev-pipeline.yaml`) runs `bash` with
`curl` + `jq` inside the assertoor container, writes to `$ASSERTOOR_SUMMARY`, and
fails the task on non-zero exit. `run_command` runs an arbitrary argv.
`run_javascript` runs JS. These are the escape hatches for anything
`check_http_json` cannot express.

**What CANNOT be done — and it is exactly our load-bearing half.** Every
`run-epbs-sim.sh` `--assert` mode except `preserve` depends on **grepping
container logs** (`kurtosis service logs -a` against `cb-epbs` and
`cl-1-lodestar-geth`). Assertoor has **no task that reads a container's logs**:
its world is beacon/execution API endpoints and arbitrary HTTP, plus a shell
that has enclave *network* access but **not** the Docker socket or the
`kurtosis` CLI. It therefore cannot see:

- `auction winner ... buildoor-mux` (CB won the auction);
- `Ignoring p2p bid below min bid ... minBid=200000000` (the p2p floor fired);
- `Selected builder block ... bidSource=...cb-epbs` vs `...p2p`;
- `OffsetOutOfBounds` (the SSZ decode over-read on preset mismatch);
- `Responded with <code> ... method=/eth/v1/builder/beacon_blocks` (the reveal
  access log — the comment at line 253 notes this image logs NOTHING else for
  the reveal, so the access-log line IS the only signal);
- `auth signature verification failed` / `AuthSigVerify`;
- `no header available` (204) and `get_execution_payload_bid failed ... Internal`
  (500) from the CB bid endpoint.

Some of these have an HTTP-observable proxy assertoor *could* reconstruct
(e.g. builder-down could be tested by stopping buildoor out-of-band and asserting
the chain keeps producing value==0 blocks; a p2p floor could in principle be
inferred if buildoor's p2p bid value were exposed on chain). But the crisp,
cheap signals our modes rely on are log lines, and porting them to assertoor
means either (a) re-deriving the same fact from an HTTP/on-chain observable, or
(b) giving the assertoor container a way to read logs (Docker socket mount) that
the ethereum-package does not provide and that we should not add lightly.

**Portability verdict per mode:**

| `--assert` mode | Portable to assertoor? | How |
|---|---|---|
| default (builder-built blocks) | **Yes** | `check_http_json`/`run_shell` on `/eth/v2/beacon/blocks` value!=0 |
| `preserve` | **Yes** | `check_http_json`/`run_shell` GET+POST the keymanager `builder_config` API (no logs) |
| `builder-down` | **Partial** | chain-liveness + value==0 on chain are HTTP-observable; the "CB returns 204 never 500" half is a log signal, needs the CB status endpoint to expose it or stays bespoke |
| `p2p` | **No (as written)** | the floor-fired + bidSource signals are BN log lines only |
| `block-submission` | **No (as written)** | the reveal decode 2xx/4xx + OffsetOutOfBounds are CB/BN log lines only |
| `request-auth` | **No (as written)** | AuthSigVerify is a CB log line; only the "loop still produces blocks" half is on-chain |

---

## 5. What each side gives that the other cannot

**Assertoor gives us, that the bespoke harness does not:**

1. **Cross-client coverage, cheaply.** `run_task_matrix` + the ethereum-package
   `participants_matrix` produce per-CL pass/fail across lodestar / lighthouse /
   teku / nimbus / grandine from ONE args file. `run-epbs-sim.sh` is
   lodestar-only by construction (fixed service-name discovery; only
   `local/lodestar:km` carries the keymanager `builder_config`).
2. **A declarative, ethpandaops-maintained gloas test corpus.** The 15 gloas-dev
   playbooks track the spec (`process_previous_payload` ordering, index reuse,
   worst-case block, EIP-8282 contracts). We get spec-conformance regressions
   for free and stay in sync as gloas evolves.
3. **A UI/dashboard + HTTP API + standard exit-code CI surface.** Our harness
   prints a colored PASS line; assertoor gives a browsable per-task report and a
   machine-readable API.
4. **Chain-health assertions we don't have** — finality under load, reorg
   counts, attestation stats, sync status, missed-slot rate.

**The bespoke harness gives us, that assertoor cannot (today):**

1. **The commit-boost-in-loop assertions.** The whole point of cb-testing is that
   the block flows *through commit-boost*. The gloas-dev playbooks test VC ->
   buildoor **direct** and never touch CB. Only `run-epbs-sim.sh` inserts CB and
   runs `cb-km apply`.
2. **The log-grep signals** (auction winner, p2p floor rejection, reveal decode,
   AuthSigVerify, 204/500 accounting) — see section 4. These are precisely the
   properties of *our* code (CB's bid pipe, SSZ decode, request-auth domain,
   failover) and they have no assertoor-native expression.
3. **The keymanager `builder_config` projection** (`cb-km apply`,
   `--preserve-entries`). This is our tooling, driven out-of-enclave.

**The one-line framing:** assertoor tests **the network and the generic gloas
builder role**; `run-epbs-sim.sh` tests **our commit-boost code in the builder
loop**. They do not overlap; they stack.

---

## 6. Integration proposal (tiered)

### Tier 1 — cross-client baseline via the ethpandaops playbooks (adopt now)

`configs/epbs/gloas-epbs-matrix.yaml` already implements this: a
`participants_matrix` of five CLs on `glamsterdam-devnet-8` images,
`mev_type: buildoor`, `additional_services: [assertoor, dora]`, and
`assertoor_params.tests` pointing at the raw builder-lifecycle + builder-deposit
playbooks. `gloas_fork_epoch: 1` (not 0) so the lifecycle playbook can exercise
its pre-fork onboarding branch.

To make it a one-command capability, add a `just` recipe and a short doc pointer:

```make
# Cross-client gloas builder-flow baseline via ethpandaops assertoor.
# VC -> buildoor DIRECT (no commit-boost). Reports per-CL pass/fail in the
# assertoor UI. See docs/assertoor-investigation.md.
epbs-matrix enclave="epbs-matrix":
    kurtosis run github.com/ethpandaops/ethereum-package \
      --enclave {{enclave}} \
      --args-file configs/epbs/gloas-epbs-matrix.yaml \
      --image-download always
    @echo "assertoor + dora UIs:"
    @kurtosis enclave inspect {{enclave}} | grep -E 'assertoor|dora' || true
```

Cost: one extra devnet (five CLs + assertoor + dora), no bespoke code. It runs
standalone (no `run-epbs-sim.sh`). **Must be documented as a NON-commit-boost
test** so nobody reads a green matrix as "our CB code passed."

Version pinning: today the matrix references the playbooks by **raw master URL**
(`.../refs/heads/master/playbooks/gloas-dev/...`), which is a moving target — a
change upstream can silently change what our baseline asserts. Recommendation:
**vendor the two playbooks** into `assertoor/gloas-dev/` (as we already vendor
`assertoor/cb-mev-pipeline.yaml`) and point `tests: [{file: ...}]` at the local
copies, or pin the URL to a commit SHA rather than `master`. Vendoring also lets
us patch `builderCount` / timing for our preset without forking upstream.

### Tier 2 — a custom `commit-boost-builder-flow.yaml` playbook (spike, partial)

A custom playbook can mirror the HTTP-observable subset of our `--assert` modes.
Sketch (approximate task list):

```yaml
id: commit-boost-builder-flow
name: "Commit-Boost gloas builder flow (CB in the loop)"
timeout: 60m
config:
  cbUrl: "http://cb-epbs:18550"
  beaconUrl: ""          # auto-discovered
  minBuilderSlots: 8
tasks:
  - name: check_clients_are_healthy
    config: { minClientCount: 1 }

  # CB is up and answering its PBS status endpoint
  - name: check_http_json
    title: "commit-boost PBS is reachable"
    config:
      url: "http://cb-epbs:18550/eth/v1/builder/status"
      expectStatus: 200
      failOnCheckMiss: true

  # wait past buildoor activation
  - name: check_consensus_slot_range
    config: { minSlotNumber: 40 }

  # builder-built blocks on chain (the default-mode assertion, portable)
  - name: run_shell
    title: ">= minBuilderSlots builder-built blocks (value != 0)"
    config:
      envVars: { BEACON_URL: "beaconUrl", MIN: "minBuilderSlots" }
      command: |
        set -eo pipefail
        head=$(curl -sf "$BEACON_URL/eth/v1/beacon/headers/head" | jq -r .data.header.message.slot)
        built=0
        for s in $(seq $((head-16)) $head); do
          v=$(curl -sf "$BEACON_URL/eth/v2/beacon/blocks/$s" \
              | jq -r '.data.message.body.signed_execution_payload_bid.message.value // "0"')
          [ "$v" != "0" ] && built=$((built+1))
        done
        echo "builder_built=$built" >> "$ASSERTOOR_SUMMARY"
        [ "$built" -ge "$MIN" ] || { echo "FAIL only $built builder-built"; exit 1; }
```

Feasible in this playbook: CB reachability, on-chain builder-built count, and (by
POSTing the keymanager API from `run_shell`) a `preserve`-style
`builder_config` survival check. **Not feasible without a `run_shell` escape
hatch that can read logs (which it cannot):** the auction-winner attribution, the
p2p floor rejection, the `/beacon_blocks` reveal decode class, AuthSigVerify, and
the 204/500 accounting. Those stay in `run-epbs-sim.sh` unless commit-boost grows
HTTP/metrics endpoints that expose the same facts (e.g. a Prometheus counter for
reveal-decode failures, auction source, request-auth failures — which
`check_http_metrics` could then assert natively). That is the real unlock for
Tier 2 and is a commit-boost change, not an assertoor one.

Note the enclave-integration friction: `run-epbs-sim.sh` inserts CB and runs
`cb-km apply` AFTER `kurtosis run`, out-of-band. Assertoor tasks run *inside*
`kurtosis run`. So a Tier-2 playbook would either need CB + `cb-km apply` already
wired into the args file (the native `epbs` mev_type work that docs/EPBS.md
documents as blocked on an upstream ethereum-package change), or it would run as
a *second* assertoor invocation pointed at the enclave after our script has set
CB up. The cleaner path is to keep the CB-in-loop assertions in the script and
use assertoor only where it needs no post-launch wiring.

### Tier 3 — replace or sit alongside?

**Sit alongside. Do not replace `run-epbs-sim.sh`.** The script is the only thing
that (a) puts commit-boost in the loop, (b) runs `cb-km apply`, and (c) asserts
the log-level signals that are the actual regression surface for our code.
Assertoor cannot do any of the three today. The right division of labor:

- `run-epbs-sim.sh` + `gloas-epbs.yaml` / `gloas-epbs-2cl.yaml` = the
  **commit-boost regression gate** (lodestar, CB in loop, log asserts).
- `gloas-epbs-matrix.yaml` + assertoor = the **cross-client + chain-health
  baseline** (all CLs, generic gloas builder role, no CB).

Revisit replacement only if commit-boost exposes its internal signals over
HTTP/metrics (then Tier 2 could grow to cover most modes natively).

---

## 7. Gaps and risks

- **The ethpandaops playbooks do NOT test our code.** Restating because it is the
  easiest mistake to make: `mev_type: buildoor` is VC -> buildoor direct.
  commit-boost is not in that loop. A green `gloas-epbs-matrix` says "these CLs
  implement the gloas builder role," not "commit-boost works." Every doc/recipe
  around Tier 1 must say this.
- **Moving-target playbooks.** The raw `master` URLs in `gloas-epbs-matrix.yaml`
  can change under us. Vendor them (as with `cb-mev-pipeline.yaml`) or pin to a
  SHA. (Recommendation in Tier 1.)
- **No log access = the load-bearing asserts don't port.** Assertoor sees APIs,
  not container logs. Six of our seven assertion signals are log lines. This is
  the structural reason for "alongside, not replace."
- **`--network none` hermeticity.** Assertoor's power tasks pull remote
  resources: `generate_child_wallet` prefunds from chain (fine, in-enclave), but
  the matrix `tests` are fetched over the network from GitHub, and CL/EL/buildoor
  images are pulled. So an assertoor matrix run is **not** hermetic unless the
  playbooks are vendored AND all images are pre-pulled. Our bespoke harness has
  the same non-hermeticity (it launches upstream `EP_PACKAGE` over the network),
  so this is not a regression, but neither path is offline-clean today. If we
  ever need `--network none`, vendoring the playbooks is a prerequisite.
- **Resource weight.** Tier 1 adds two services per enclave (assertoor + dora)
  plus a five-CL matrix. That is materially heavier than the single-lodestar
  `gloas-epbs.yaml`. On the shared box (RAM contended with a Kurtosis eth-sim
  agent per the operator notes) check free RAM before running the matrix, and do
  not run it concurrently with `run-epbs-sim.sh` (the script already assumes one
  ~15G devnet at a time). Assertoor and dora themselves are light; the CL matrix
  is the cost.
- **A live experiment is running on this box.** Nothing in Tier 1 should be
  launched until that clears; this report is analysis only.

---

## 8. Recommendation

1. **Adopt Tier 1 now.** Add the `just epbs-matrix` recipe, **vendor the two
   gloas-dev playbooks** into `assertoor/gloas-dev/` (or pin to a SHA), and add a
   one-paragraph section to `docs/EPBS.md` that says clearly: this is the
   cross-client baseline, VC->buildoor direct, NOT a commit-boost test.
2. **Do a small Tier-2 spike** for the two modes that port cleanly (default
   builder-built count, and `preserve` via the keymanager API) as a
   `commit-boost-builder-flow.yaml`, so we have a declarative record of the
   HTTP-observable CB assertions. Do not try to port the log-grep modes.
3. **Keep `run-epbs-sim.sh` as the authoritative CB gate** (Tier 3: alongside).
4. **File a commit-boost enhancement** to expose auction-source / reveal-decode /
   request-auth / bid-status counters over HTTP or Prometheus. That single change
   is what would let assertoor's `check_http_json` / `check_http_metrics` absorb
   most of our `--assert` modes and make a fuller Tier-2 (or eventual Tier-3
   replacement) feasible.

---

## Appendix — assertoor task types relevant to us

From `pkg/tasks/*/task.go` (53 total; the relevant subset):

**Chain-health / consensus checks**
- `check_clients_are_healthy` — at least N clients ready (`minClientCount`).
- `check_consensus_sync_status` / `check_execution_sync_status` — CL/EL synced.
- `check_consensus_finality` — chain finalizing (`minFinalizedEpochs`,
  `maxUnfinalizedEpochs`).
- `check_consensus_slot_range` — wait for / assert a slot or epoch range
  (`minSlotNumber`, `minEpochNumber`); also reports `currentSlot`/`currentEpoch`.
- `check_consensus_builder_status` — **gloas-specific**: assert a builder pubkey/
  index is active (`expectActive`) or exiting (`expectExiting`), with
  balance bounds (`minBuilderBalance`/`maxBuilderBalance`). The core assert in
  builder-lifecycle.
- `check_consensus_block_proposals`, `check_consensus_attestation_stats`,
  `check_consensus_reorgs`, `check_consensus_forks`, `check_consensus_validator_status`,
  `check_consensus_proposer_duty`, `check_consensus_identity`,
  `check_consensus_api` — chain-behavior asserts.

**HTTP / EL assertion (the CB-relevant ones)**
- `check_http_json` — GET/POST/etc. an arbitrary URL with headers/body, assert
  status code(s) and JSON `assertions`, with polling. Can address `cb-epbs:18550`
  by enclave DNS. The native way to assert commit-boost's PBS API.
- `check_http_metrics` — assert Prometheus metrics from an endpoint (the future
  unlock for CB internal counters).
- `check_eth_call` / `check_eth_config` — EL eth_call assertions / EL config.

**Generation (state-changing)**
- `generate_builder_deposits` — EIP-8282 builder deposit (raw calldata,
  `DOMAIN_BUILDER_DEPOSIT`).
- `generate_builder_exits` — EIP-8282 builder exit request.
- `generate_deposits` / `generate_batch_deposits` — standard validator deposits
  (used for the pre-fork 0xB0 onboarding path).
- `generate_exits`, `generate_bls_changes`, `generate_consolidations`,
  `generate_slashings`, `generate_withdrawal_requests`, `generate_attestations`,
  `generate_transaction` / `generate_eoa_transactions` /
  `generate_blob_transactions`.

**Data / setup**
- `get_consensus_specs` — read chain specs (`GLOAS_FORK_EPOCH`,
  `SLOTS_PER_EPOCH`, `MIN_BUILDER_WITHDRAWABILITY_DELAY`, ...).
- `get_pubkeys_from_mnemonic`, `get_random_mnemonic`, `generate_child_wallet`,
  `get_wallet_details` — key material and funded wallets.
- `get_consensus_block_header`, `get_consensus_validators`,
  `get_consensus_proposer_duties`, `get_execution_block`.

**Control flow / escape hatches**
- `run_tasks` — ordered sub-tasks.
- `run_tasks_concurrent` — parallel sub-tasks.
- `run_task_matrix` — run one task over a list (per-CL, per-builder fan-out).
- `run_task_options` / `run_task_background` — options wrapper / background task.
- `run_shell` — run bash (`curl`+`jq`), write `$ASSERTOOR_SUMMARY`, fail on
  non-zero exit. The escape hatch used in `assertoor/cb-mev-pipeline.yaml`. Has
  enclave network access but NOT the Docker socket / kurtosis CLI (cannot read
  container logs).
- `run_command` — run an arbitrary argv (`allowed_to_fail`).
- `run_javascript` — run JS.
- `run_external_tasks` — pull tasks from an external source.
- `sleep` — delay.

**Sources.** `scripts/run-epbs-sim.sh`; `configs/epbs/gloas-epbs.yaml`,
`gloas-epbs-2cl.yaml`, `gloas-epbs-matrix.yaml`; `docs/EPBS.md`;
`assertoor/cb-mev-pipeline.yaml`; assertoor repo README; raw playbooks
`playbooks/gloas-dev/builder-lifecycle.yaml` and `builder-deposit.yaml`;
`pkg/tasks/*/task.go` (task catalog) and the `config.go` structs for
`check_http_json`, `run_command`, `check_consensus_builder_status`.

**Flagged as unverified.** I did not launch any enclave (a live experiment is
running), so the claim that assertoor's `run_shell` container lacks the Docker
socket / kurtosis CLI is inferred from the ethereum-package service model and the
task catalog (no log-reading task exists), not confirmed on a running enclave. If
Tier 2 is pursued, verify by exec-ing into the assertoor container and checking
for `docker` / `kurtosis` and socket access before assuming the log-grep modes
are impossible.
