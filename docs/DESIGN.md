# cb-testing — design

Why this project exists, what it is trying to be, and the engineering laws it holds to. Read this when
a change feels like it is fighting the grain. `docs/ARCH.md` is the companion HOW (module map, seams);
this doc is the WHY and the principles everything else cites as "Law N".

## What cb-testing is

An **opinionated block-building simulation substrate for [Commit-Boost](https://github.com/Commit-Boost/commit-boost-client)**
— the layer that stands up a real Ethereum devnet with a relay + builder + commit-boost sidecar,
exercises a specific PBS/ePBS feature end to end, and returns a trustworthy verdict you can gate a
release on. Nothing here is a mock; the point is not "the devnet booted," it is a verdict.

Mainline `ethpandaops/ethereum-package` treats out-of-protocol block building as a bespoke, hard-coded
convenience; it has no incentive to keep commit-boost first-class, and **ePBS will churn exactly this
surface** (relay / builder / sidecar wiring). So cb-testing maintains it, opinionated about
commit-boost, and aims to be the integration layer the release flow never had.

## The problem it solves

The release loop for a sidecar like this is otherwise: unit tests → ship → wait for testnet feedback →
release. Weak coverage, slow. The integration layer that should sit in the middle is this repo, but the
earlier version was built before test-driven discipline, so it was neither trustworthy nor fast enough
to lean on. As ePBS clients ship, the issue rate rises, and the human-in-the-loop version (paste
terminal output, `docker logs` a container by hand, isolate the real panic under three masking errors)
does not scale. **The win is a loop that can be driven end to end: launch → triage → diagnose → report,
autonomously — legible to a human and to an automated agent from the same output.**

## The thesis (the mechanism, not a rewrite)

The leverage is NOT "rewrite it in Rust." It is three properties, in priority order:

1. **Preflight against the real images.** Validate a rendered config by having the actual relay /
   commit-boost image parse it in ~1s, before any 10-minute devnet spend. This is the generalized,
   productized form of the manual `docker run --rm <helix:main>` loop that first found real schema
   drift. It is the single biggest legibility win and it retires the whole class of "schema drift
   discovered 5 minutes into a launch as a masked runtime panic."
2. **Observable by default (not agent-only tooling).** Legibility is a system property: structured
   `tracing` events on everything, a durable verdict report, and AUTOMATIC root-cause capture on any
   failure, emitted as the normal output of a normal run. A human reads a pretty rendering of that
   stream; an automated consumer reads the JSON; ONE source of truth, not a separate agent surface. So
   root-cause capture is a PROPERTY OF THE RUN (when a service dies, the harness attaches its
   container's root panic to the event stream automatically) — the `triage` verb is only the
   after-the-fact entry point into that same data, not the mechanism. This is strictly better for a
   human too (root panic inline vs `docker logs` by hand).
3. **Feature-asserting scenarios.** Every scenario positively asserts its feature's codepath FIRED
   (skip-sigverify counter > 0, timing-game poll count, extra-validation RPC hit, cross-relay best-bid
   comparison), not merely "the pipeline didn't crash." A scenario that passes while the feature
   silently no-oped is a non-test.

Type-reuse from the real config crates is a supporting move (see Law 1), valuable but scoped: it makes
commit-boost config drift a compile error; it does NOT solve helix drift (Law 1 caveat). Preflight is
what covers both halves.

## Design laws

Non-negotiable; each prevents a named smell. Numbered and quotable — other docs and code cite them as
"Law N."

<a id="law-1"></a>
### Law 1 — Configs come from the real schemas, never hand-mirrored strings

A string-template generator that reverse-engineers a service's serde layout from binary panics is the
root smell and does not survive. Commit-boost config is built from `cb_common` config structs and
serialized, so a renamed field is a compile error / `deny_unknown_fields` deserialize error.

*Caveat:* helix types are NOT reusably importable (divergent branch, different org, a `teloxide` type
graph), and helix `CoresConfig` is the field that drifts most. So helix gets an owned, thin
`HelixRelayConfig` mirror pinned in lockstep with the `HELIX_RELAY_IMAGE` tag, guarded by the Preflight
law. The few kurtosis-runtime template holes (`{{ .Timestamp/.Port/.Network/.Relays }}`) are the one
place string-patching survives; isolate them. (Note: because a hand-written mirror is still
hand-mirrored, preflight — not the mirror — is the real guard for helix; see Law 1's interaction with
the config-generation history in `docs/ARCH.md` §4.)

<a id="law-2"></a>
### Law 2 — No config, image name, or scenario truth exists in two hand-edited places

One image map (no four-way `commit-boost/pbs` vs `commit-boost/commit-boost` drift). No checked-in
stale example config that hand-copies generator output; generate on demand instead.

<a id="law-3"></a>
### Law 3 — Every scenario asserts its feature fired

No green from mere block delivery. A pass-gate must key on the signal that proves the feature ran
(e.g. `pubkeys_verified`, not raw `total_events`); a best-bid check must compare bid values across
relays, not union-by-slot (one delivering relay must not pass identically to genuine two-relay
aggregation). A harness that lies green is worse than an ugly one.

<a id="law-4"></a>
### Law 4 — Verdict logic is unit-tested and TDD-able without a devnet

The checks are pure functions over already-fetched data; inject fixture beacon/relay responses and
test the math. Every judgement splits into a pure classifier (data in, verdict out, unit-tested on both
sides of every boundary) and a thin I/O shell holding zero verdict logic. CI runs the generator and
schema-validates every output.

<a id="law-5"></a>
### Law 5 — Observability is a first-class system property, not an agent bolt-on

Everything emits structured `tracing` events + a durable verdict report; failures auto-attach their
root cause to that stream. Humans and automated consumers tap the SAME surface (pretty rendering vs
JSON) — never a separate agent-only tool category; build good logging on everything and let both
consume it. The `VerificationReport` is the model to extend, not replace.

<a id="law-6"></a>
### Law 6 — Dogfood the abstraction in one fork; upstreaming is optional icing later

The `(relay, sidecar, builder)` component model (`mev_resolver.star`) + the `mev_type: custom` config
API already exist in the fork, and cb-testing already consumes them — so this is maturing what exists,
not a new build, and there is no rush to upstream. Topology: ONE fork carries the abstraction;
cb-testing is its consumer/dogfood (do NOT maintain two forks — the abstraction-vs-usage seam is the
fork/consumer boundary, not two forks of one repo). "Do it properly" means:

- (a) refine the abstraction to be clean and GENERAL (reads like an API a stranger would use: any relay
  × any sidecar × any builder, already spans epbs/buildoor/mev-rs);
- (b) add the external-sidecar hook (let VCs point at an externally-supplied builder URL; make each
  component independently `none`-able) — the one missing piece that later enables a thin
  compose-over-UNMODIFIED-upstream shim;
- (c) keep the fork delta minimal and PR-shaped as you touch it.

OPTIONAL LATER: upstream the proven code (a medium PR — the `mev_resolver.star` module + a refactor of
`main.star`'s mev dispatch + `input_parser.star`'s per-client builder-flag matrix), then cb-testing
repoints its `import_module` from the fork to upstream in one line. A pure shim is not possible on
today's upstream because upstream injects the VC `--builder` flag inside `enrich_mev_extra_params`,
triggered only by a native `mev_type` via a naming-convention URL, with no external-builder hook — a
shim would otherwise reimplement the brittle per-client flag matrix, worse than the fork. (The `#1384`
"exit" referenced in earlier audits is UNVERIFIED — check upstream HEAD before any PR.)

<a id="law-7"></a>
### Law 7 — Coverage is a matrix, not a point

Scenarios parametrize over EL/CL client pairs. A regression specific to nethermind+prysm is invisible
if everything hardcodes geth+lighthouse.

## Where to go next

- `docs/ARCH.md` — how the pieces fit (module map, the config↔fork seam, the verdict model).
- `docs/CHECKS.md` — the per-check catalog (tiers, thresholds, feature-assertion status).
- `docs/DEVELOPING.md` — the dev loop + how to add a check or a scenario.
- `docs/fork-delta.md` — what the `ethereum-package` fork changes vs upstream, file by file.
- `docs/local-kurtosis-e2e.md` — the operational runbook.
