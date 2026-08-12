# Composable scenarios — design

Status: design (implementation on `feat/composable-scenarios`). This doc is a PROPOSAL; on ship, mark it
landed and point it at the implementing files.

## Problem

A test scenario is one hardcoded `Scenario` enum variant mapped to one frozen golden YAML fixture (byte-diff
acceptance). Features (WS header-stream, timing-games, extra-validation, skip-sigverify, min-bid, mux /
multi-relay topology, EL/CL client pair, signer) are not composable: producing "WS + prysm + timing-games"
needs a whole new fixture and a new enum variant. The ask: make scenarios composable, drivable by a structured
(AI-targetable) surface, and enumerable/testable — without a combinatorial pile of golden fixtures.

## The design (post-grill: 3 design agents + 3 adversarial lenses)

**`ScenarioSpec`** — a flat struct of closed enums / `Option`s is the single source of truth. It is the surface
a caller (human, or an agent emitting JSON) targets; it is `lower()`'s input; its `armed_features()` is the
verifier oracle. Closed enums make illegal values inexpressible; **smart constructors** make the three known
illegal combinations unrepresentable, so there is no `validate()` returning conflicts — illegal states don't
compile.

Axes (each maps onto an EXISTING seam, nothing new is emitted):
- `clients: ClientPair {GethLighthouse, NethermindPrysm}` → `ElCl::{DEFAULT, ALT}`
- `topology: Topology {Single, TwoRelays, DivergentRelays, Mux}` — relay count + subsidy intent in one knob
- `get_header: HeaderTransport {Http, Stream{api_key: Present|Absent}}` — Absent = the ws-nokey negative control
- `timing_games: bool` (timeouts 400/2000 ride it), `extra_validation: bool`, `signer: bool`
- `sigverify: Sigverify {On, Skip, SkipPoisoned, PoisonedControl}` — collapses the mutually-exclusive combos
- `min_bid: MinBid {None, Floor(f64)}` — subsidy is DERIVED (Floor ⇒ subsidy 0), never a spec field

**`lower(spec, images, keys_dir) -> args_file`** — deterministic, total on any constructible spec. Reuses
verbatim: `CbParams` + its `extra_pbs_lines`/`per_relay_lines` `Vec<String>` seams, `cb_toml`, `cb_toml_mux`
(mux is a dedicated exclusive branch — structurally a different template), `build_mev_params`, `ElCl`,
`poisoned_relay_url`, `WRONG_RELAY_PUBKEY`, `load_pubkeys`. Subsidy, timeouts, and network_params are DERIVED
inside `lower` (they are non-orthogonal couplings each scenario bundles — see Honesty note). The seam-line
fragments are composed in a FIXED canonical order, pinned by a dedicated composite-spec test (below).

**Acceptance model** (byte-golden evolves, does not die):
- The 13 named scenarios: `NAMED: &[(&str, ScenarioSpec)]` const table. `every_scenario_matches_its_golden`
  byte-diffs `lower(named)` against the frozen golden (UNCHANGED `assert_matches_golden`). This is the
  migration safety net AND the regression anchor. `Scenario::from_name`/`ALL` stay working, backed by `NAMED`.
- The combinatorial space is NEVER byte-goldened. Two OFFLINE guards over an enumeration of the pruned product:
  - `every_pruned_spec_renders_without_panic` — `lower` is total across the legal space.
  - **round-trip**: `detect_enabled_features(lower(spec)) == spec.armed_features()`. Documented as a
    RENDERER-DRIFT guard ONLY — it proves emit↔detect agree, NOT that the config is valid CB (both sides share
    the same key strings, so a shared typo passes; `[pbs]` has no `deny_unknown_fields`). Real config
    validation is `sim preflight` (Law 1), which callers run before a live run.
  - **composite-spec fragment-order pin**: a unit test asserting `to_cb_params()` output for a COMPOSITE spec
    (e.g. timing+extra-validation) has the exact expected line order. The 13 goldens each pin ONE order; only a
    composite test catches a canonical-order regression on combinations they don't cover.

**Driving surface** (the "run me a scenario like X with Y and Z"):
- `sim generate --base <name> --set k=v,...` — a deterministic keyword overlay: start from a named base, apply
  typed field updates, render. Zero model. This is the composability UX.
- `sim generate --spec <file.json>` — render a full `ScenarioSpec` supplied as JSON (`deny_unknown_fields`).
  This is the AI-driven entry: an agent (Lisa, chat) composes the spec and passes it; validity is by
  construction because output comes from `lower()`.

The system is **AI-driven by construction** — the structured surface IS what an agent targets — without a
brittle in-binary LLM/NL parser. A natural-language front-end (`sim scenario "<english>"`) is a thin, optional
add on top of this surface; it is deliberately deferred (see Cut list) until the deterministic core is proven.

## Cut list (grill-driven — what we deliberately did NOT build)

- **No `sim matrix` live-sweep verb / no "N/M pass" coverage integer.** At ~10 min/cell live the sweep has no
  consumer, and a single pass-tally conflates config-rendered / never-run / expectation-downgraded cells — the
  coverage-theater trap. Enumeration ships as offline tests only. If genuine cross-situation coverage is wanted,
  the right form is promoting a few high-value combos to NAMED, runnable, byte-goldened scenarios (below).
- **No 4-valued `Expectation` / `expected_checks(spec)`.** "Proven" is a RUNTIME outcome (a marker fires only
  if a bid/getHeader lands in the window; min-bid rejection needs `rejections>0`), so a spec cannot soundly
  declare it. Gating `--require-feature-proof` off a per-spec expectation table re-hardens what the classifiers
  softened to WARN and manufactures flaky reds. `--require-feature-proof` keeps deriving from the RENDERED
  config as it does today. The spec declares only `armed_features()` (2-valued: armed / not).
- **No `Conflict`/`validate()`.** Smart constructors make illegal combos unrepresentable; `min_bid ⇒ subsidy 0`
  is a derivation, not a reportable clamp.
- **No in-binary NL/AI layer, no schemars overlay type** yet. Deferred behind the deterministic surface.

## High-value un-named combos (the right "enumerate situations")

Rather than a Cartesian sweep, promote a curated handful of genuinely-interesting combos to NAMED scenarios,
each a real falsifiable question worth a devnet spend (authored with a golden once run):
- **ws × nethermind-prysm** — Law 7's exact concern (a prysm-specific regression is invisible under hardcoded
  geth+lighthouse); the highest-suspicion route coupling.
- **timing_games × extra_validation** — the one genuine composition claim (both markers must fire; the seam
  lines must compose in canonical order without clobbering).
- **poison × nethermind-prysm** — the skip_sigverify differential on the ALT client pair.
- **min_bid (Floor) × nethermind-prysm** — the silent-ignore canary (`[pbs]` no `deny_unknown_fields`) against
  a real CB parse.

## Honesty note (orthogonality is partly fiction)

The struct advertises a product space, but `lower` only honors a subregion: mux composes with nothing (dedicated
template), min-bid/poison require Single relay, subsidy/timeouts/network_params are derived not chosen. This is
real and stated: the win is that the illegal region is made unrepresentable by construction (smart constructors)
rather than absent-from-a-match, and that the 13 named scenarios are reproduced byte-for-byte through the new
path. We do NOT market "features combine freely."

## Ship order

1. `ScenarioSpec` + smart constructors + `lower` + `armed_features` + the 13-named migration (byte-golden net).
2. Offline guards: round-trip + `renders_without_panic` + composite fragment-order pin.
3. `sim generate --base/--set` and `--spec` driving surface.
4. (Follow-up) promote the 4 curated combos to named+goldened scenarios after a live run each.
5. (Deferred) NL front-end, if demand.
