# cb-testing — contributor guide

How to get the repo running and how to extend it — add a **check** (a new verdict on the pipeline) or a
**scenario** (a new devnet configuration that exercises a CB feature). This is the how-to-develop doc; it
does not re-explain the architecture or the check catalog:

- **[`docs/ARCH.md`](ARCH.md)** — how the pieces fit (module map, the config↔fork seam, the verdict model).
- **[`docs/CHECKS.md`](CHECKS.md)** — the authoritative per-check catalog + the verdict contract for consumers.
- **[`docs/DESIGN.md`](DESIGN.md)** — why the repo exists + the design laws referenced below (Law 1
  real-schema configs, Law 3 feature-asserting scenarios, Law 4 TDD-able verdicts, Law 5 observability).
- **[`docs/local-kurtosis-e2e.md`](local-kurtosis-e2e.md)** — the operational runbook + kurtosis pin.

---

## 1. Dev loop / prerequisites

### Toolchain

- **Rust 1.91+**, edition 2024 (`Cargo.toml` pins `rust-version = "1.91"`). No Node, no bun — this is a pure
  Rust workspace (one lib + five bins: `cb-verify`, `cb-orchestrator`, `sim`, `test-mux`, `test-relay`).
- **Docker + Kurtosis CLI 1.18.1** — only needed for the devnet e2e, not for unit tests. Pin 1.18.1 (the
  parsers read its human text tables; a newer CLI has a config-version clash — see the runbook).
- The bundled submodules (`git submodule update --init`, or clone `--recursive`): the forked
  `ethereum-package` (needed to launch a devnet), plus `commit-boost-client` and `helix` (build sources).

### The `just` recipes (the whole dev loop)

`just` is the task runner (`justfile` at the repo root). The inner loop is pure-Rust and hermetic — no
Docker, no network, sub-second:

| Recipe | What it runs | When |
|---|---|---|
| `just check` | `cargo check --all-targets` | fast compile check, no codegen |
| `just test` | `cargo test` | all unit tests — **pure, fast, hermetic** (no devnet, no docker) |
| `just fmt` / `just fmt-check` | `cargo fmt` / `cargo fmt --check` | format / CI format gate |
| `just clippy` | `cargo clippy --all-targets -- -D warnings` | strict lint (**warnings are errors**) |
| `just lint` | `fmt-check` + `clippy` | the pre-commit gate |
| `just ci` | `check` + `test` + `lint` | the full local CI pipeline — run this before pushing |
| `just build-release` | `cargo build --release` | release binary |
| `just generate-configs` | `sim generate` | regenerate `configs/generated/*.yml` from the typed model |

The two gates that CI enforces (mirror them locally): **`cargo fmt --check`** and **`clippy -D warnings`**.
A `just ci` green means the pure surface is good; it says nothing about the devnet path.

### Unit tests vs the devnet e2e

- **`cargo test` / `just test` — the everyday loop.** Every verdict, parser, and config assembler has a pure
  core that is unit-tested against fixture data (`tests/fixtures/`, `#[cfg(test)]` modules in each source
  file). No devnet, no Docker, no images. This is where you do TDD. Runs in seconds.
- **`just e2e` — the full devnet confirmation.** Regenerates configs, pulls the public images, launches a
  Kurtosis enclave (geth + lighthouse + N helix relays + reth-rbuilder + the CB sidecar + dora/spamoor/
  prometheus), observes ~1 epoch, runs every check, prints the report. Needs Docker + Kurtosis 1.18.1 and a
  **locally-built CB image** — run `just build-cb-image` once first (it builds `commit-boost/commit-boost:kurtosis`
  from the bundled `./commit-boost-client` submodule; the helix + reth images are public and pulled). A run is
  ~10 minutes; it is the *final* confirmation, never the debugger. See [`docs/local-kurtosis-e2e.md`](local-kurtosis-e2e.md).

Middle ground: `just test-mux`, `just verify-now`, and `sim preflight <config>` (the ~1s real-image config
probe) run against a live enclave without a full observation window — useful for iterating on a running devnet.

---

## 2. How to add a CHECK

A check is a pure verdict over already-fetched pipeline data. The pattern (**DESIGN Law 4** — "verdict
logic is TDD-able without a devnet"; the check-trustworthiness plan calls it the `classify_*` seam):

### The two-part shape

**(a) Write the pure classifier — the seam.** A free function that takes *already-fetched* data (maps,
counts, parsed log events — never a network client) and returns a `CheckResult`:

```rust
pub fn classify_<name>(data: &AlreadyFetched) -> CheckResult { … }
```

`CheckResult` and its constructors live in [`src/checks/mod.rs`](../src/checks/mod.rs):
`CheckResult::{pass, fail, warn, skip}(id, tier, detail)` plus `.with_data(json)`. `id` is the check's stable
name (also its key in the JSON report), `tier ∈ {1,2,3}` (§ tier choice below), `detail` is a human string,
`data` is a `serde_json::Value` for machine consumers. `CheckStatus` serializes UPPERCASE under the JSON key
`result` (`PASS`/`FAIL`/`WARN`/`SKIP`).

Make the classifier generic over the value type where it helps testing — e.g.
`classify_payload_matches<H>` takes `BTreeMap<u64, Vec<(String, H)>>` so tests pass a trivial `u64` in place
of a real `B256` hash.

**(b) Unit-test both sides of every boundary.** For each Pass/Warn/Fail/Skip transition, write a test with
fixture data that lands just inside and just outside the boundary. The worked example
[`src/checks/payload_matching.rs`](../src/checks/payload_matching.rs) tests: clean single-relay match → PASS,
cross-relay conflict → WARN, no-relay-matches-chain → WARN, missed-not-downgraded → PASS, empty → SKIP. Build
the fixture maps with small helpers (`by_slot(...)`, `chain(...)`) so each test is one line of intent.

**(c) Write the thin async fetch shell.** A separate `async fn` that does the I/O (calls `BeaconClient` /
`RelayClient` / metrics / `kurtosis logs`), assembles the same data shape, and calls the classifier. It holds
**no verdict logic** — it just gathers and delegates. In `payload_matching.rs` that is
`check_payload_hash_match(...)`: fetch per-(relay,slot) hashes + on-chain hashes, then `classify_payload_matches(...)`.

**(d) Wire it into `run_verification`.** In [`src/main.rs`](../src/main.rs) (~lines 414-526) the checks are
collected into one `Vec<CheckResult>`. Add your fetch fn there, either via a module `run_*` that returns
`Vec<CheckResult>` (`all_checks.extend(...)`, as chain_health / relay_pipeline / payload_matching /
cb_metrics do) or a single `all_checks.push(...)` (as best_bid and mux_routing do). Register the module in
`src/checks/mod.rs` if it is new.

### Worked examples (read these, don't invent a new shape)

- **[`src/checks/cb_metrics.rs`](../src/checks/cb_metrics.rs)** — `collect_endpoint_stats(scrape, endpoint) →
  EndpointStats` (pure gather over parsed Prometheus) then `classify_endpoint(endpoint, &stats, strict) →
  CheckResult` (pure verdict, ~19 decision tests, no known false-greens). The cleanest seam in the repo.
- **[`src/checks/mux_routing.rs`](../src/checks/mux_routing.rs)** — `parse_cb_log_line` +
  `extract_mux_from_config` + `classify_mux_routing` (pure); the async shell fetches CB logs.
- **[`src/checks/best_bid.rs`](../src/checks/best_bid.rs)** — `classify_best_bid` (+ `value_eth_to_wei`),
  fed by offered bids parsed from CB's own getHeader log events.

### Pick the tier deliberately

The tier is the **severity contract** — it decides whether your check can fail the run (full table:
[`docs/CHECKS.md`](CHECKS.md)):

- **Tier 1 = must** — a real pipeline invariant. A tier-1 `FAIL` fails the whole run (exit code 1). Use only
  for "the pipeline is genuinely broken."
- **Tier 2 = should** — a health signal. Never fails the run on its own. (`cb_metrics` matrix checks are the
  one exception: authored tier 2 but *escalated to tier 1 on FAIL* because a relay 5xx is a real failure.)
- **Tier 3 = informational** — annotative only.

Note the trust rule: a check that exists to *catch an anomaly* should prefer **WARN**, not a silent PASS,
when it could not actually verify anything (the P3 false-green fixes: `mux.routing`, `payload_hash_match`,
`relay.best_bid` all WARN rather than pass-on-nothing). WARN is non-fatal, so surface it in `data` and let the
consumer gate on the JSON `result` (§4).

### The anti-pattern (why the seam is non-negotiable)

**Do not weld the verdict into the async fetch fn.** `chain_health` and `relay_pipeline` inline their verdicts
in the async check fns and have *no* factored-out classifier — which is exactly the standing gap the
check-trustworthiness plan exists to close. A verdict
tangled with `await` calls cannot be unit-tested without a devnet, so its pass/fail boundaries go unproven —
which is how false-greens ship. Pure classifier first, thin I/O shell second, always.

---

## 3. How to add a SCENARIO

A scenario is a typed devnet configuration that assembles into a Kurtosis args-file. Everything lives under
[`src/bin/sim/genmodel/`](../src/bin/sim/genmodel/); the assembly is pure and guarded by byte-identity golden
fixtures. The config↔fork coupling (the two `|` block scalars, the runtime template holes) is explained in
[`docs/ARCH.md`](ARCH.md) §4 — read it before touching the block bodies.

### Steps

1. **Add the variant** to the `Scenario` enum in
   [`src/bin/sim/genmodel/scenario.rs`](../src/bin/sim/genmodel/scenario.rs), and add it to `Scenario::ALL`
   (order = emission order — match the intent; the array is what `sim generate` iterates).
2. **Fill the match arms** for the new variant: `name()` (the canonical basename, e.g. `cb-myfeature`),
   `comment()` (the leading doc block), `relays()` (`&["helix"]` single-relay vs `&["helix", "helix"]`
   multi-relay — this also toggles the scalar-vs-list `mev_relay` form and `mev_relay_image` emission),
   `cb_block()` (the CB TOML — see next step), and `network_params()` (only if you need a different validator
   count; `Mux` is the sole scenario using `MUX_NETWORK_PARAMS` = 256 keys).
3. **Build the CB TOML.** Most scenarios just construct a `CbParams` in `cb_block()` and call
   `cb_toml(&params)` — the knobs are `timeout_get_header_ms`, `timeout_get_payload_ms`, `extra_pbs_lines`
   (injected into `[pbs]`), `per_relay_lines` (injected inside the `{{ range }}` relay loop). See
   [`src/bin/sim/genmodel/cb.rs`](../src/bin/sim/genmodel/cb.rs): skip-sigverify adds one `[pbs]` line,
   extra-validation adds two, timing-games sets short timeouts + three per-relay lines. Only add a whole new
   builder (like `cb_toml_mux`) when the TOML *structure* changes (mux moves the range loop and adds `[[mux]]`
   blocks). Generate-time knobs are injected by plain string-building, **not serde** — no quoting/sentinel hazard.
4. **The helix block is shared.** `HELIX_RELAY_CONFIG` in
   [`src/bin/sim/genmodel/helix.rs`](../src/bin/sim/genmodel/helix.rs) is byte-identical across all scenarios.
   Only touch it if the *relay* config itself must change — and if you do, keep it pinned in lockstep with the
   `HELIX_RELAY_IMAGE` tag (Law 1 caveat: helix types aren't importable, so this const *is* the contract) and
   preserve the hard-won comments verbatim.
5. **Regenerate + add the golden fixture.** Run `just generate-configs`, then copy the new
   `configs/generated/cb-myfeature.yml` to `tests/fixtures/golden-configs/cb-myfeature.yml`. The test
   `every_scenario_matches_its_golden` (in `scenario.rs`) then asserts `sim generate` reproduces it
   **byte-for-byte** with the default images.
6. **The drift gate.** `sim generate --check` (`generate::check`) is the CI/agent form of the same guard — it
   fails if the on-disk configs no longer match the generator. Run it after any generator change.

### Two caveats

- **Multi-relay goldens are self-generated.** The byte-identity oracle only proves the generator reproduces
  *its own* output; it does **not** prove the config is a valid, working devnet. Validate a new scenario on a
  real devnet: `sim preflight configs/generated/cb-myfeature.yml` (~1s real-image parse) and then
  `just e2e configs/generated/cb-myfeature.yml`.
- **Assert the feature fired (Law 3).** A scenario that passes while its feature silently no-oped is a
  non-test. A new scenario should ship with a check that *positively asserts its codepath fired* — a
  skip-sigverify counter > 0, a timing-game poll count, an extra-validation RPC hit — not just the generic
  pipeline checks. Today only `mux.routing` and `relay.best_bid` do this (the gap is documented in
  [`docs/CHECKS.md`](CHECKS.md) "Known gaps"); adding the assertion is § 2 above.

---

## 4. The verdict contract (for consumers)

A run emits a `VerificationReport` (human-rendered, or `--json`) and an exit code. **The exit code keys only
on a tier-1 FAIL**: `0` = no tier-1 failure, `1` = some tier-1 check FAILed, `2` = no tier-1 check ran at all
(a setup/discovery/preflight failure). `WARN` and `SKIP` are **non-fatal at every tier** and never move the
exit code. The load-bearing consequence: several checks that exist to catch a real anomaly report it as
`WARN` (relay equivocation, unverifiable mux routing, best-bid shortfall) — a run that hits them **still exits
0**. A CI job or agent that cares about those must **parse the JSON and inspect each check's `result` field**,
not gate on the exit code alone. Full contract, per-check pass/warn/fail conditions, and the one tier-2→tier-1
escalation: [`docs/CHECKS.md`](CHECKS.md).

---

## 5. Repo map — where things live

Do not reverse-engineer the tree; the module-by-module map is **[`docs/ARCH.md`](ARCH.md) §2–3** (shared lib
`src/lib.rs`, the `cb-verify` binary `src/main.rs`, the `sim` submodules under `src/bin/sim/`, and the
config↔fork seam). The check catalog is [`docs/CHECKS.md`](CHECKS.md); the fork divergence is
[`docs/fork-delta.md`](fork-delta.md); the current backlog of what to build next is the internal
the local `.agent/` working area (backlog + plans index).
