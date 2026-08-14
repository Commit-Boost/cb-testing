# Composable scenarios — design

Status: implemented (`feat/composable-scenarios`, `genmodel/spec.rs` + `sim scenario`). Ship-order steps 1-3
landed; step 4 (promote curated combos to named+goldened) and step 5 (NL front-end) are follow-ups.

## Validation (live)

- Offline: `lower_reproduces_every_scenario` proves `render(spec) == args_file_in` byte-for-byte for all 13
  named scenarios; round-trip + composite-order + total-render property tests green; full suite + clippy
  `-D warnings` clean.
- Real-schema: a novel compose (`get_header=stream` + `clients=nethermind-prysm` + `timing_games`) passes
  `sim preflight` (helix config-parse against the real image).
- End-to-end: a novel compose (`cb-timing-games` base + `extra_validation`, two-relays — no named scenario,
  no golden) rendered by `sim scenario`, stood up a full Kurtosis devnet, and BOTH composed features were
  positively proven from CB debug logs: `feature.timing_games` PASS (385 marker lines),
  `feature.extra_validation` PASS (126 marker lines); overall exit 0. The two WARNs (get_header deadline
  timeouts, p95 latency) are the expected artifacts of aggressive timing games, not failures.

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
  This is the AI-driven entry: an AI agent or chat UI composes the spec and passes it; validity is by
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

## Client coverage (the `clients` axis)

The `clients` axis is the Law-7 matrix. CLs are the axis that matters for CB behavior (the blinded-block /
get_header flow), so the additional pairs vary the CL against geth; `nethermind-prysm` keeps its historical EL.
`ClientPair` variants: `geth-lighthouse` (default), `nethermind-prysm`, `geth-teku`, `geth-nimbus`,
`geth-lodestar` — i.e. **all 5 mainstream CLs** (lighthouse, prysm, teku, nimbus, lodestar). Adding a client is
an `ElCl` + `ClientPair` variant + serde name; the rpc_url naming (`el-1-{el}-{cl}`) is already parametric.

## Curated coverage points (the right "enumerate situations")

Rather than a Cartesian sweep, `spec::curated()` freezes a handful of genuinely-interesting composed specs as
named+goldened regression anchors (`tests/fixtures/curated-configs/`), each **live-validated on a devnet**
before its golden is trusted (a golden of a config that has never run is worthless). Emit them with
`sim generate --curated`.

| Curated point | Why | Live result |
|---|---|---|
| `cb-basic-teku` | teku CL (Law 7) | 14 PASS / 0 WARN / 0 FAIL; 33 payloads, 100% MEV |
| `cb-basic-nimbus` | nimbus CL (Law 7) | 14 / 0 / 0; 31 payloads, 93.9% MEV |
| `cb-basic-lodestar` | lodestar CL (Law 7) | 14 / 0 / 0; 31 payloads, 93.9% MEV |
| `cb-ws-prysm` | ws stream on prysm — the highest-suspicion route coupling | 15 / 1 / 0; **ws stream FIRED** (30 headers, 1 startup-race fallback) — the coupling concern is refuted by measurement |
| `cb-timing-extra-validation` | the composition claim (both markers must fire) | both `feature.timing_games` + `feature.extra_validation` proven from CB logs |

Not curated as ws×CL points: `poison × prysm` and `min_bid × prysm` were dropped on reassessment —
skip_sigverify and min_bid are CB-internal (the CL never participates; the `commit_boost_config` block is
byte-identical across CLs), so a client pairing adds no Law-7 coverage that `cb-sigverify-diff` / `cb-min-bid`
don't already have.

### The ws header stream requires a helix built from the `./helix` submodule (not the public `:main` image)

An initial ws×CL sweep appeared to show the stream failing on teku/nimbus/lodestar while working on
lighthouse/prysm — but that was **not** CL-dependent. Root cause: the devnet pulls the public helix image
`ghcr.io/gattaca-com/helix-relay:main`, and in helix `main` the header-stream admission is **stubbed** — the
`ApiProvider::admit_header_stream` trait method has a default that unconditionally returns
`Err("header stream not available")`, and the open-source `DefaultApiProvider` does not override it (its
`get_preferences` even hard-codes `api_key: None`). The real admission logic lives in gattaca's private
`ApiProvider`, not shipped in the public build. So `:main` refuses the stream for **every** proposer, any CL.

The lighthouse/prysm runs "worked" only because they ran against an **older** `:main`: the image is a mutable
tag, and it was rebuilt with the stub between those runs and the teku/nimbus/lodestar ones (confirmed by the
image's build timestamp straddling the success/failure boundary, the running digest, and that `main` has exactly
one `admit_header_stream` — the stub, no override). The vendored `./helix` submodule (`develop`) still carries
the **working** public admission (`header_stream.rs` calls `check_api_key`, and `get_preferences` reads the
`x-api-key` header), which is why helix is vendored.

**To run any ws scenario: build helix from the submodule and point the devnet at it.**

```bash
just build-helix-image                          # -> local/helix-relay:kurtosis (from ./helix)
echo 'HELIX_RELAY_IMAGE=local/helix-relay:kurtosis' >> .env
just e2e configs/generated/cb-ws-stream.yml     # or any composed ws scenario
```

The ws curated point (`cb-ws-prysm`) and the named `cb-ws-stream` / `cb-ws-stream-nokey` scenarios are only
reproducible against a submodule-built helix; against the current public `:main` they degrade to HTTP fallback.
This is a mutable-tag skew trap: pinning `:main` while also vendoring the source meant an upstream rebuild could
silently disable a feature under test. The durable fix is to build helix from the submodule for ws (above); a
`develop`-tracking pin or a specific working digest are alternatives.

**Confirmed** (2026-08-13): `sim scenario --set clients=geth-teku,get_header=stream` — the exact CL that
"failed" against `:main` — streams cleanly against the submodule build: `feature.ws_header_stream` PASS
(37 CB proof lines), `feature.ws_stream_fallback` PASS (zero HTTP fallbacks), 33 payloads delivered, 100% MEV.
Proof it was never CL-dependent.

**Submodule-helix data-api caveat (now hardened):** the `relay.validator_registrations` check queries the
relay's `/relay/v1/data/validator_registration` data-api, which the `develop` build answers from an unpopulated
postgres backing (the in-memory cache that the admission path uses *is* populated — the ws stream authenticated).
So it reports `0/128` even though registrations worked. The check is now hardened: when it sees `0/N` *and* the
relay confirmed delivery (`relay.payloads_delivered_multi` PASS — a relay only delivers to registered
proposers), it SKIPs with an explanation instead of FAILing. A genuine total-registration failure (0 registered,
0 delivered) still FAILs, and is independently caught by the tier-1 delivery check.

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
