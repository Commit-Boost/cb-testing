# cb-testing — NORTH STAR

Why this repo exists and where it's going. Read when a change feels like it's fighting the grain.
Terse on purpose. The companion runbook is `docs/local-kurtosis-e2e.md`; the paid-for incident it
records is the evidence base for half of what follows.

## The mission
Own the **opinionated block-building simulation substrate** for commit-boost — the layer that stands up
a real Ethereum devnet with a relay + builder + commit-boost sidecar, exercises a specific PBS/ePBS
feature end to end, and returns a trustworthy verdict. Mainline `ethpandaops/ethereum-package` treats
out-of-protocol block building as a bespoke, hard-coded convenience; it has no incentive to keep
commit-boost first-class, and **ePBS will churn exactly this surface**. So we maintain it, opinionated
about commit-boost, and we make it good enough to be the integration layer the release flow never had.

## The problem we are actually solving
Today's release loop is: unit tests -> ship -> wait for testnet feedback -> release. Weak coverage, slow.
The integration layer that should sit in the middle exists (this repo) but was built before agent/TDD
discipline, so it is not trustworthy or fast enough to lean on. As ePBS clients ship, the issue rate
spikes and the human-in-the-loop version (paste terminal output, `docker logs` a container by hand,
isolate the real panic under three masking errors) does not scale. **The win is a loop an agent can
drive: launch -> triage -> diagnose -> report, autonomously.**

## The thesis (the mechanism, not the rewrite)
The leverage is NOT "rewrite in Rust." It is three properties, in priority order:

1. **Preflight against the real images.** Validate a rendered config by having the actual relay /
   commit-boost image parse it in ~1s, before any 10-minute devnet spend. This is the generalized,
   productized form of the manual `docker run --rm <helix:main>` loop that found today's drift. It is
   the single biggest agent-friendliness win and it retires the whole class of "schema drift discovered
   5 minutes into a launch as a masked runtime panic."
2. **Observable by default (NOT agent-only tooling — J 2026-07-30).** The goal is legibility as a system
   property: structured `tracing` events on everything, a durable verdict report, and AUTOMATIC root-cause
   capture on any failure, emitted as the normal output of a normal run. A human reads a pretty rendering
   of that stream; an agent reads the JSON; ONE source of truth, not a separate agent surface. So
   root-cause capture is a PROPERTY OF THE RUN (when a service dies, the harness attaches its container's
   root panic to the event stream automatically) — the `triage` verb is only the after-the-fact entry
   point into that same data, not the mechanism. Agents are the most demanding consumers of good
   observability; design for observability and the agent affordance comes free. This is also strictly
   better for a human (root panic inline vs `docker logs` by hand).
3. **Feature-asserting scenarios.** Every scenario positively asserts its feature's codepath FIRED
   (skip-sigverify counter > 0, timing-game poll count, extra-validation RPC hit, cross-relay best-bid
   comparison), not merely "the pipeline didn't crash." A scenario that passes while the feature
   silently no-oped is a non-test. [[verify-treatment discipline]]

Type-reuse from the real config crates is a supporting move (see Law 1), valuable but scoped: it makes
commit-boost config drift a compile error; it does NOT solve helix drift (Law 1 caveat). Preflight is
what covers both halves.

## Design laws (non-negotiable; each prevents a named smell)
1. **Configs come from the real schemas, never hand-mirrored strings.** The Python string-template
   generator (`generate_kurtosis_configs.py`, which reverse-engineers helix's serde layout from binary
   panics) is the root smell and dies. Commit-boost config is built from `cb_common` config structs and
   serialized (a renamed field is then a compile error / `deny_unknown_fields` deserialize error).
   *Caveat:* helix types are NOT reusably importable (divergent branch, different org, a `teloxide`
   type graph) — and helix `CoresConfig` is the field that drifts most. So helix gets an owned, thin
   `HelixRelayConfig` mirror pinned in lockstep with the `HELIX_RELAY_IMAGE` tag, guarded by the
   Preflight law. The ~4 kurtosis-runtime template fields (`{{ .Timestamp/.Port/.Network/.Relays }}`)
   are the one place string-patching survives; isolate them.
2. **No config, image name, or scenario truth exists in two hand-edited places.** One image map (kill
   the four-way `commit-boost/pbs:kurtosis` vs `commit-boost/commit-boost:kurtosis` drift). Delete the
   checked-in `example-kurtosis-config.yml` (an already-stale hand-copy of generator output) in favor
   of generate-on-demand.
3. **Every scenario asserts its feature fired.** No green from mere block delivery. Fix the mux
   pass-gate to key on `pubkeys_verified`, not `total_events` (today it reports "all routing verified"
   having verified zero decisions when CB debug logging is off). Fix the best-bid check to compare bid
   values across relays, not union-by-slot (today one delivering relay passes identically to genuine
   two-relay aggregation).
4. **Verdict logic is unit-tested and TDD-able without a devnet.** The checks are pure functions over
   already-fetched data; inject fixture beacon/relay responses and test the math
   (`chain_health`/`payload_matching`/`relay_pipeline`/`check_mux_routing` have zero decision-logic
   tests today). CI runs the generator and schema-validates every output.
5. **Observability is a first-class system property, not an agent bolt-on (J 2026-07-30).** Everything
   emits structured `tracing` events + a durable verdict report; failures auto-attach their root cause to
   that stream. Humans and agents tap the SAME surface (pretty rendering vs JSON) — never build a separate
   agent-only tool category; build good logging on everything and let both consume it. cb-verify's
   `VerificationReport` is the model to extend, not replace.
6. **Dogfood the abstraction in our ONE fork; upstream is optional icing later (RATIFIED, J 2026-07-30).**
   The `(relay, sidecar, builder)` component model (`mev_resolver.star`) + the `mev_type: custom` config
   API ALREADY exist in our fork, and cb-testing ALREADY consumes them — so this is maturing what exists,
   not a new build, and there is NO rush to upstream. Topology: ONE fork carries the abstraction; cb-testing
   is its consumer/dogfood (do NOT maintain two forks — the abstraction-vs-usage seam is the
   fork/consumer boundary, not two forks of one repo). "Do it properly" = (a) refine the abstraction to be
   clean + GENERAL (reads like an API a stranger would use: any relay x any sidecar x any builder, already
   spans epbs/buildoor/mev-rs), (b) add the external-sidecar hook (let VCs point at an externally-supplied
   builder URL; make each component independently `none`-able) — the one missing piece that later enables a
   thin compose-over-UNMODIFIED-upstream shim, (c) keep the fork delta minimal + PR-shaped as you touch it.
   OPTIONAL LATER: upstream the proven code (a MEDIUM PR — the `mev_resolver.star` module + refactor of
   `main.star`'s mev dispatch + `input_parser.star`'s per-client builder-flag matrix), then cb-testing
   repoints its `import_module` from our-fork to upstream in one line. WHY a pure shim isn't possible on
   TODAY's upstream: upstream injects the VC `--builder` flag inside `enrich_mev_extra_params`, triggered
   only by a native `mev_type` via a naming-convention URL, with no external-builder hook — a shim would
   otherwise have to reimplement the brittle 7-client flag matrix, worse than the fork. (The `#1384` "exit"
   from an earlier audit is UNVERIFIED — no reference exists in the fork; check upstream HEAD before any PR.)
7. **Coverage is a matrix, not a point.** Scenarios parametrize over EL/CL client pairs. A CB regression
   specific to nethermind+prysm is invisible today (everything hardcodes geth+lighthouse).

## Architecture target
A single Rust application (`sim`) — library-first, `clap` CLI, `tracing` JSON:
`sim generate | preflight | run | verify | triage`. It shells `kurtosis` (no Rust SDK exists; enclave
ops stay text-parsing with `--format json` where available — inherent, not fixable by the rewrite). The
mature `src/` verifier (discovery/beacon/relay/metrics/checks/report) moves in nearly verbatim. Config
generation folds in from Python; orchestration folds in from `run-and-verify.sh` + `cb-orchestrator`
(one entry at `--jobs 1`); the assertoor duplicate and the shell launcher retire. Language count drops
4 -> 2 (Rust + the unavoidable starlark fork).

**Home (RATIFIED, J 2026-07-30): stays in the cb-testing repo** — its own repo, `cb-common` pinned by
git tag to the release under test. NOT embedded as a commit-boost-client workspace crate (embedding
solves CB drift, which was not the pain — helix was, and helix types can't live in the CB workspace
anyway — while dragging kurtosis/docker/testnet weight into commit-boost's CI), and NOT a new repo.
A tag-dep expresses "test exactly this release"; recover early-CB-drift signal with a nightly lane that
builds the harness against `cb main`.

**Rust scope (RATIFIED direction, J 2026-07-30): full Rust.** The whole harness consolidates into the
single `sim` app — generation + orchestration folded in, Python/shell/assertoor retired. Build the new
P1 verbs (`preflight`, `triage`) directly as Rust so they are the first slices of `sim`, not throwaway.

## Keep / kill
- KEEP: the `cb-verify` core (JSON report, tiered checks, Tier-0 reachability preflight, Postgres
  post-mortem). It is the healthy part.
- KILL: the Python string-template generator; the checked-in stale example config; the shell launcher;
  the assertoor second verification harness; the duplicated diagnostic bins that re-implement checks.

## Staged plan
- **P0 (done today):** pin the working matrix (kurtosis 1.18.1 + the 3 helix fixes) and land the runbook.
  Commit today's fixes (helix launcher wait-for-genesis in the fork; config-schema reconciliation +
  runbook in cb-testing).
- **P1 — the agent loop (highest leverage, independent):** `sim preflight <config>` (render + real-image
  parse, ~1s) and auto-triage (dump every non-RUNNING service's logs + root panic into the JSON report
  on any failure, including launch-phase). Wraps the EXISTING setup; unblocks everything else. Do first.
- **P2 — LANDED (as consolidation, NOT typed mirrors):** three adversarial grills + a direct diff killed
  the "build CB config from `cb_common` structs / owned typed helix mirror" plan — the "6 duplicated
  templates" premise was false (helix is byte-identical across all 6 scenarios; CB varies <=7 lines), a
  helix mirror gains no compile guard (types aren't importable), and the serde mechanism is fragile
  (sentinel collisions). Instead the Python generator was PORTED VERBATIM into `sim generate` (const
  templates; the `{{ }}` runtime holes the ethereum-package fills at launch stay literal), with typing
  only at the assembly layer (a `Scenario` enum + one `Images` map that fixed the live-wrong
  `commit-boost/pbs` default). Byte-identity to golden is the oracle. Details +the grill trail:
  `docs/plans/P2-consolidate-config-gen.md`. **NOTE for J:** this outcome tensions with Law 1's premise
  ("built from `cb_common` structs ... a renamed field is a compile error") — a hand-written mirror is
  still hand-mirrored and preflight stays the only real guard; consider revising Law 1's mechanism claim.
- **P3 — coverage as assertions + TDD:** every scenario asserts its feature fired (Law 3); unit-test the
  verdict math (Law 4); add one alternate EL/CL pair (Law 7). Each new CB feature ships with its sim
  scenario, like a unit test.
- **P4 — fork diet + treadmill:** `upstream` remote + tagged pin; shrink overlay toward just
  `mev_resolver.star`; a routine `bump` workflow (update CLI -> rebase fork -> run sweep -> diff verdicts
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
- RATIFIED (J 2026-07-30): home stays in cb-testing repo; full-Rust `sim` consolidation is the direction.
- STILL OPEN: whether the fork investment is worth it long-term vs waiting on ethpandaops#1384 (the north
  star currently bets on owning it; revisit if #1384 lands or the rebase cost climbs).
