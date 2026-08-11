# cb-testing — NORTH STAR (internal: plan + scars)

Internal working ledger. The durable WHY (mission, thesis) and the 7 design laws are the PUBLIC
`docs/DESIGN.md` — this file is the staged plan, the ratified directions, the keep/kill list, and the
scars/open-decisions that are not yet settled enough to be public. When a plan lands, its status of
record is `.agent/plans/INDEX.md`; this ledger lags reality and defers to that index.

## Architecture target

A single Rust application (`sim`) — library-first, `clap` CLI, `tracing` JSON:
`sim generate | preflight | run | verify | triage`. It shells `kurtosis` (no Rust SDK exists; enclave
ops stay text-parsing with `--format json` where available — inherent, not fixable by the rewrite). The
mature `src/` verifier (discovery/beacon/relay/metrics/checks/report) moves in nearly verbatim. Config
generation folds in from the retired Python generator; orchestration folds in from `run-and-verify.sh` +
`cb-orchestrator` (one entry at `--jobs 1`); the assertoor duplicate and the shell launcher retire.
Language count drops 4 → 2 (Rust + the unavoidable starlark fork).

**Home (ratified): stays in the cb-testing repo** — its own repo, `cb-common` pinned by git tag to the
release under test. NOT embedded as a commit-boost-client workspace crate (embedding solves CB drift,
which was not the pain — helix was, and helix types can't live in the CB workspace anyway — while
dragging kurtosis/docker/testnet weight into commit-boost's CI), and NOT a new repo. A tag-dep expresses
"test exactly this release"; recover early-CB-drift signal with a nightly lane that builds the harness
against `cb main`.

**Rust scope (ratified direction): full Rust.** The whole harness consolidates into the single `sim`
app — generation + orchestration folded in, Python/shell/assertoor retired. Build the new verbs
(`preflight`, `triage`) directly as Rust so they are the first slices of `sim`, not throwaway.

## Keep / kill

- KEEP: the `cb-verify` core (JSON report, tiered checks, Tier-0 reachability preflight, Postgres
  post-mortem). It is the healthy part.
- KILL: the Python string-template generator; the checked-in stale example config; the shell launcher;
  the assertoor second verification harness; the duplicated diagnostic bins that re-implement checks.

## Staged plan

- **P0 (done):** pin the working matrix (kurtosis 1.18.1 + the 3 helix fixes) and land the runbook.
  Committed: helix launcher wait-for-genesis in the fork; config-schema reconciliation + runbook.
- **P1 — the agent loop (highest leverage, independent):** `sim preflight <config>` (render + real-image
  parse, ~1s) and auto-triage (dump every non-RUNNING service's logs + root panic into the JSON report
  on any failure, including launch-phase). Wraps the EXISTING setup; unblocks everything else. LANDED —
  see `.agent/plans/P1-sim-preflight-triage.md`.
- **P2 — LANDED (as consolidation, NOT typed mirrors):** three adversarial grills + a direct diff killed
  the "build CB config from `cb_common` structs / owned typed helix mirror" plan — the "6 duplicated
  templates" premise was false (helix is byte-identical across all 6 scenarios; CB varies ≤7 lines), a
  helix mirror gains no compile guard (types aren't importable), and the serde mechanism is fragile
  (sentinel collisions). Instead the Python generator was PORTED VERBATIM into `sim generate` (const
  templates; the `{{ }}` runtime holes the ethereum-package fills at launch stay literal), with typing
  only at the assembly layer (a `Scenario` enum + one `Images` map that fixed the live-wrong
  `commit-boost/pbs` default). Byte-identity to golden is the oracle. Details + grill trail:
  `.agent/plans/P2-consolidate-config-gen.md`. **Open tension:** this outcome tensions with Law 1's
  premise ("built from `cb_common` structs … a renamed field is a compile error") — a hand-written mirror
  is still hand-mirrored and preflight stays the only real guard; consider revising Law 1's mechanism
  claim in `docs/DESIGN.md`.
- **P3 — coverage as assertions + TDD:** every scenario asserts its feature fired (Law 3); unit-test the
  verdict math (Law 4); add one alternate EL/CL pair (Law 7). Each new CB feature ships with its sim
  scenario, like a unit test. LANDED — see `.agent/plans/P3-check-trustworthiness.md`.
- **P4 — fork diet + treadmill:** `upstream` remote + tagged pin; shrink overlay toward just
  `mev_resolver.star`; a routine `bump` workflow (update CLI → rebase fork → run sweep → diff verdicts
  for regressions), agent-runnable.
- **P5 — consolidate:** fold orchestration into `sim`; retire shell/justfile/assertoor duplication.
- **Later — ePBS:** an ePBS-aware relay/builder + a gloas ethereum-package + the epbs-branch CB image;
  a new `cb_get_execution_payload_bid` check arm + an envelope-flow beacon check. The component model
  (Law 6) is what makes swapping in an ePBS builder tractable.

## Scars & open decisions

- Helix type-reuse is impractical (proven: local checkout's `CoresConfig` already mismatches deployed
  `:main`). The mirror + preflight is the mitigation; do not git-dep gattaca's `helix-common`.
- Kurtosis has no Rust SDK; enclave discovery is brittle text-parsing either way.
- `cb-common` is a heavy dep (alloy-full + lighthouse git tags + blst); a separate testing repo eats the
  cold-build cost.
- RATIFIED: home stays in cb-testing repo; full-Rust `sim` consolidation is the direction.
- STILL OPEN: whether the fork investment is worth it long-term vs waiting on ethpandaops#1384 (the
  current bet is on owning it; revisit if #1384 lands or the rebase cost climbs).
