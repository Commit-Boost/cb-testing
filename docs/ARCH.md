# cb-testing — ARCHITECTURE

How the pieces fit together. `docs/DESIGN.md` says WHY this repo exists and where it's going;
this doc is the HOW — the map a newcomer needs so they don't have to reverse-engineer ~10k lines of
Rust. Keep it small and load-bearing: WHAT each thing does lives in the code + tests; WHY a given
design was chosen lives here and in `docs/DESIGN.md`.

Scope note: the target is a single library-first Rust app (`sim generate | preflight | run | verify |
triage`). Today it is **most** of the way there — the verifier, config generation, preflight, and
triage are all Rust; a shell launcher (`run-and-verify.sh`) still glues launch+verify together and is
the last thing slated to fold into `sim run`. This doc describes what exists now and flags the seams.

---

## 1. End-to-end flow

One devnet run, from typed config to verdict. Names below are the actual binaries / scripts /
functions.

```
 sim generate                         (Rust, src/bin/sim/generate.rs → genmodel/*)
   │  typed Scenario + Images → args_file_in() assembles the Kurtosis YAML
   │  writes configs/generated/<scenario>.yml  (byte-identical to golden fixtures)
   ▼
 just e2e  /  just testnet  ──►  scripts/run-and-verify.sh   (the attached-mode launcher)
   │
   ├─(0) cargo build --release --bin cb-verify      pre-build OFF the critical path
   │                                                (no multi-GB compile while 10 services are live)
   ├─(1) kurtosis enclave rm -f <enclave>           clear any stale enclave
   ├─(1b) sim preflight <config>                    src/bin/sim/preflight.rs
   │        render.rs pulls the two | block scalars, substitutes dummy runtime vars,
   │        runs the REAL helix image against the rendered config (~1s docker probe),
   │        classify_helix_probe() → Pass / Fail{field} / Inconclusive.
   │        Exits nonzero ONLY on Fail (genuine config drift) → aborts the run early.
   ├─(1c) check_host_memory                         advisory OOM warning before a ~10-min spend
   ├─(2) kurtosis run ./ethereum-package \          launch the devnet (the forked package)
   │        --args-file <config> --image-download always
   │        → geth + lighthouse, N helix relays, reth-rbuilder builder,
   │          commit-boost sidecar, dora + spamoor + prometheus
   │        on launch failure → sim triage <enclave>  (root-cause capture, then exit)
   ▼
 cb-verify --enclave <enclave> --config <config> …   (src/main.rs; the pre-built binary)
   │  discovery::discover()      kurtosis inspect/port print → beacon/relay/cb/metrics URLs
   │  health::probe_all()        Tier-0 reachability preflight (dead service → abort or postmortem)
   │  wait_for_slot()            wait until head passes the observation window's end slot
   │  checks::*                  chain_health, relay_pipeline, payload_matching, best_bid,
   │                             cb_metrics, mux_routing → Vec<CheckResult>
   │  report::print_report() + exit_code()   verdict keyed on any Tier-1 FAIL
   ▼
 VerificationReport (human render OR --json)   +   exit 0 / 1 / 2
```

On any service crash mid-run, or a launch failure, `sim triage` (`triage.rs` → `diagnose.rs`)
attaches each dead service's root panic to the output — observability as a property of the run, not a
separate tool (DESIGN Law 5).

`just` verbs are thin wrappers: `generate-configs` → `sim generate`; `e2e`/`testnet` →
`run-and-verify.sh`; `verify*` → `cb-verify` directly; `test-all` → `cb-orchestrator`.

---

## 2. Two binaries + a shared library (+ `sim`)

The crate is **library-first** (the target architecture; see `docs/DESIGN.md`): the mature verifier modules live in
`src/lib.rs` (`cb_testnet_verifier`) so every binary imports them instead of re-declaring or
re-implementing. `Cargo.toml` declares one lib and five bins.

| Target | Path | Role |
|---|---|---|
| `cb_testnet_verifier` (lib) | `src/lib.rs` | Shared modules: `beacon`, `relay`, `metrics`, `checks`, `discovery`, `report`. Imported by every bin. |
| `cb-verify` (bin, default-run) | `src/main.rs` | The single-enclave verifier. Discover → preflight → observe → run checks → report → exit code. `health`/`live` are private to this bin. |
| `cb-orchestrator` (bin) | `src/orchestrator.rs` | Parallel multi-enclave runner (`just test-all`). Spawns a launch→wait→observe→check→teardown pipeline per config, bounded by a `--jobs` semaphore, then shells the built `cb-verify` binary for the checks and aggregates a `BatchReport`. This is the "one entry at `--jobs 1`" that `sim run` will eventually absorb. |
| `sim` (bin) | `src/bin/sim/main.rs` | Separate app for `generate` / `preflight` / `triage`. Reuses the lib but owns its own submodule tree. Sync-only (see §6). |
| `test-mux`, `test-relay` (bins) | `src/bin/test_{mux,relay}.rs` | Standalone diagnostics (quick mux-routing / relay-API probes). Slated for retirement into the checks they duplicate. |

Two verdict surfaces coexist: `cb-verify`/orchestrator produce a **`VerificationReport`** (tiered
checks); `sim` produces its own small JSON reports (`PreflightReport`, `TriageReport`). They are
different report types because `sim` runs before/around a devnet, not against a healthy one.

---

## 3. Module map of `src/`

(Replaces the stale tree in `README.md`.)

### Shared library (`src/lib.rs`)

| Module | Responsibility |
|---|---|
| `discovery.rs` | Shell `kurtosis enclave inspect` / `port print`, parse the text tables → `EnclaveServices` (beacon / relay / cb-pbs / cb-metrics URLs, relay identities, cb service names, prometheus). Also `query_mev_relay_postgres` post-mortem. Pure helpers `parse_services`, `split_on_multi_space`, `extract_ports`, `matches_pattern`, `is_relay_api_service`, `relay_identity` are unit-tested. |
| `beacon.rs` | `BeaconClient` — async reqwest wrapper over the standard Beacon API (head slot, finalized epoch, syncing, genesis time, header/block-hash by slot, active validator pubkeys); returns alloy beacon types. Only the endpoints verification needs. |
| `relay.rs` | `RelayClient` — async wrapper over the Flashbots relay Data API (`ping`, cursor-paginated `get_payloads_delivered`, `get_builder_blocks_received`, `is_validator_registered`). |
| `metrics.rs` | Fetch + parse CB Prometheus text (`prometheus-parse`) via HTTP or a `kurtosis service exec` fallback; sample-aggregation helpers (`sum_metric`, `metric_values`, `has_metric`). |
| `report.rs` | `VerificationReport { enclave, timestamp, observation_window, result, checks }` + `ObservationWindow`; `print_report` (color or `--json`), `save_json_report`, and `exit_code` — the authoritative verdict→process-code mapping (see §5). |
| `checks/mod.rs` | `CheckResult { id, tier: u8, status (serde `result`), detail, data: Value }` + `CheckStatus` (Pass/Fail/Warn/Skip, serialized UPPERCASE). Constructors `pass`/`fail`/`warn`/`skip(id, tier, detail)` + `with_data`. |
| `checks/chain_health.rs` | Tier-1/2 chain checks: finality, sync status, missed-slot rate. |
| `checks/relay_pipeline.rs` | Relay-side checks: payloads delivered, builder blocks received, MEV delivery rate, validator registrations. |
| `checks/payload_matching.rs` | Tier-1: delivered payload `block_hash` matches the on-chain block. |
| `checks/best_bid.rs` | Aggregated-bidding check: compares bid values across relays (not union-by-slot — DESIGN Law 3). |
| `checks/mux_routing.rs` | Parse `[[mux]]` sections from the CB config, fetch + parse CB PBS logs (ANSI-aware), verify each proposer pubkey routed to its assigned relay. Also `fetch_service_logs` / `parse_cb_log_line`. |
| `checks/cb_metrics.rs` | CB Prometheus status-code matrices (get_header / register_validator / submit_blinded_block / status), v1→v2 fallback, get_header latency p95. Emitted at tier 2, but a `*_matrix` FAIL (a 5xx = real pipeline failure) is **escalated to tier 1** so it gates the exit code. |

### `cb-verify`-private (`src/`)

| Module | Responsibility |
|---|---|
| `health.rs` | `HealthTarget` / `ServiceKind` + `probe_all` — the Tier-0 reachability preflight and mid-wait liveness probes. |
| `live.rs` | Live-metrics deltas during the observation window (`compute_deltas`, `format_delta_{json,log}`, `LIVE_METRICS_FILTER`). |

### `sim` submodules (`src/bin/sim/`)

| Module | Responsibility |
|---|---|
| `cli.rs` | clap surface: `Cli` + `Command { Preflight, Triage, Generate }`, global `--log-format pretty|json`. |
| `main.rs` | Dispatch to the three verbs; `tracing` init; nonzero exits on error. |
| `generate.rs` | IO boundary for `sim generate`: `.env` image overrides, atomic assemble-then-write, and `--check` drift gate (`generate::check`). Pure assembly lives in `genmodel`. |
| `genmodel/mod.rs` | Golden-fixture harness (`golden`, `assert_matches_golden`, `extract_block_scalar`) — the byte-identity oracle for the verbatim config port. |
| `genmodel/scenario.rs` | `Scenario` enum (six scenarios) + `Images` map; `args_file_in()` joins the static fragments + helix const + CB block into a full args-file; `build_mev_params`. |
| `genmodel/helix.rs` | `HELIX_RELAY_CONFIG` — the helix YAML block, byte-identical across all six scenarios (const). |
| `genmodel/cb.rs` | The CB TOML block: `cb_toml(CbParams)` + `cb_toml_mux(node0, node1)` — verbatim port of the Python builders, generate-time knobs injected by string building. |
| `render.rs` | The **compatibility contract** with the fork: `extract_config_blocks` (pull the two `|` scalars), `substitute_runtime_vars` (strip the `{{ range }}` loop, fill `{{ .VAR }}` dummies), `default_dummies`. Pure. |
| `preflight.rs` | `sim preflight`: render + run the real helix image + `classify_helix_probe` (pure, 3-valued). |
| `triage.rs` | `sim triage`: `parse_service_statuses` + `services_to_triage` (pure) + process I/O to collect logs (kurtosis→docker fallback for the masking bug). |
| `diagnose.rs` | `extract_root_cause` (pure): pattern-based root-panic extraction from log text, skipping broker/grpc masking lines. Shared by `preflight` and `triage`. |

---

## 4. Config generation ↔ the fork coupling

This is the load-bearing seam. `sim generate` emits a Kurtosis args-file whose `mev_params` carries
**two `|` block scalars** that the forked ethereum-package parses and fills at launch:

- `helix_relay_config: |`  — the helix relay's YAML config (byte-identical across all six scenarios).
- `commit_boost_config: |` — the commit-boost sidecar's TOML config (varies ≤ ~7 lines per scenario).

Both blocks are **opaque scalars to YAML** and contain unrendered **Go-template holes** that only the
ethereum-package fills at `kurtosis run` time (the generator never sees the runtime values — postgres
host/port, the actual beacon/blocksim URLs, genesis timestamp, the real relay URL list):

| Block | Runtime `{{ }}` holes |
|---|---|
| helix (YAML) | `.POSTGRES_HOST_NAME`, `.POSTGRES_PORT`, `.POSTGRES_DB`, `.POSTGRES_USER`, `.POSTGRES_PASS`, `.BEACON_URI`, `.BLOCKSIM_URI` |
| commit-boost (TOML) | `.Timestamp`, `.Network`, `.Port`, and the `{{ range $index, $relay := .Relays }} … {{- end }}` loop (plus `{{ index .Relays N }}` in the mux `[[mux.relays]]`) |

**`render.rs` is the compatibility contract.** For preflight (§5), it must turn a template-holed block
back into something the real image can parse: `strip_range_blocks` drops the `.Relays` loop (valid
because relays are `#[serde(default)]` downstream) and `replace_simple_vars` fills each `{{ .VAR }}`
from `default_dummies()`. `default_dummies` therefore has to cover **every** hole the args-file uses —
if the fork adds a template var, this map is where the contract breaks, and the preflight tests
(`substituted helix is valid YAML` / `substituted CB is valid TOML`) are what catch it.

**Typing lives only at the assembly layer** (`Scenario` + `Images`), not in the block bodies. The
bodies are ported **verbatim** from the retired Python `generate_kurtosis_configs.py` into `const`
strings / string builders — the P2 grill killed the "build from `cb_common` structs / typed helix
mirror" plan (helix types aren't importable; the serde-sentinel mechanism was fragile; the templates
weren't actually duplicated). See `.agent/plans/P2-consolidate-config-gen.md`.

**The golden-fixture byte-identity guard is the oracle.** `tests/fixtures/golden-configs/<scenario>.yml`
snapshots the proven-good output; `every_scenario_matches_its_golden` asserts `sim generate` reproduces
each **byte-for-byte** with the baked-default images. A separate test guards the tracked
`configs/generated/cb-basic.yml` (which `render.rs` and `sim preflight` consume as a fixture) against
silently drifting from the generator. `sim generate --check` is the CI/agent form of the same guard.

The one image map (`Images::default()`) is also where the historical four-way
`commit-boost/pbs` vs `commit-boost/commit-boost` drift was killed; `.env` overrides are applied only
at the CLI boundary (`generate::run`), keeping assembly pure and hermetically testable.

---

## 5. The verdict model

Checks are pure functions over already-fetched data; each returns a `CheckResult { name, tier, status,
detail }` where `status ∈ {Pass, Fail, Warn, Skip}` and `tier ∈ {1, 2, 3}`. `cb-verify` collects every
check into `Vec<CheckResult>`, then the report verdict is: **`Fail` iff any Tier-1 check is `Fail`,
else `Pass`.** `report::exit_code` computes the process code by filtering to `tier == 1` checks:
**no tier-1 check present → 2** (nothing gating ran — a setup/infra failure; the discovery/preflight/
timeout paths in `main.rs` also `return 2` directly), **any tier-1 `Fail` → 1**, **else → 0**.
Tier-2/Tier-3 `Fail`s and all `Warn`s are **non-fatal**
by design; `--strict` promotes selected soft warnings to `Fail`. `Skip` never fails the run (a check
skips when its inputs are unavailable — e.g. no metrics port, no validator pubkeys, no `[[mux]]`
sections).

`sim preflight` has its own narrower 3-valued verdict (`Pass` / `Fail{field}` / `Inconclusive`) and
only its `Fail` aborts a launch — an `Inconclusive` (slow pull, docker down, pre-genesis panic) must
never be scored as config drift.

The per-check catalog (names, tiers, thresholds, what each asserts) lives in **`docs/CHECKS.md`**.

---

## 6. Key architectural decisions + rationale

- **Shell out to kurtosis, synchronously, and parse text.** There is **no Kurtosis Rust SDK**
  (`discovery.rs` TODO; only a Go SDK exists), so enclave ops are `std::process::Command` +
  text-parsing — inherent, not fixable by the rewrite. And kurtosis 1.18.1 has **no `--format json`**
  for the tables we read, so `discovery`/`triage` parse the human `enclave inspect` output column by
  column (`split_on_multi_space`). The `sim` app is deliberately **sync / no-tokio** to match this
  (verbs shell out and block; a bounded `timeout` coreutil supplies the wall-clock cap that sync std
  lacks). `cb-verify` and `cb-orchestrator` *are* tokio (they do concurrent HTTP polling / parallel
  enclaves), but `sim` and the shared discovery layer are not.

- **The forked `ethereum-package` exists on purpose.** Upstream treats out-of-protocol block building
  as bespoke, hard-coded convenience and injects the VC `--builder` flag deep inside
  `enrich_mev_extra_params` via a naming-convention URL, with no external-builder hook. The fork carries
  a general `(relay, sidecar, builder)` component model (`mev_resolver.star`) + a `mev_type: custom`
  config API — which is exactly what lets a config say "helix relay + commit-boost sidecar +
  reth-rbuilder builder" and, later, swap in an ePBS builder. A pure shim over today's upstream would
  have to reimplement a brittle 7-client flag matrix — worse than the fork. cb-testing is the fork's
  consumer/dogfood (DESIGN Law 6). ONE fork; do not maintain two.

- **The pure `classify_*` / pure-core seam.** Every verb that makes a judgement splits into a **pure
  classifier** (data in, verdict out — unit-testable against fixture logs, no devnet, no docker) and a
  thin **process-I/O shell** (smoke-checked by hand with the real tools). Named seams:
  `preflight::classify_helix_probe`, `triage::{parse_service_statuses, services_to_triage}`,
  `diagnose::extract_root_cause`, `payload_matching::classify_payload_matches`,
  `best_bid::classify_best_bid` (+ `value_eth_to_wei`), `mux_routing::classify_mux_routing` (+
  `parse_cb_log_line`, `extract_mux_from_config`), `cb_metrics::{collect_endpoint_stats,
  classify_endpoint, check_v2_fallback, check_relay_latency, histogram_quantile}`, and
  `live::compute_deltas`. These pure cores (generic over the hash/value type, taking pre-fetched
  `BTreeMap`s) are the unit-tested surface; the fixtures under `tests/fixtures/` are the real test
  inputs. This is DESIGN Law 4 ("verdict logic is TDD-able without a devnet"). The two exceptions:
  `chain_health` and `relay_pipeline` inline their verdicts in the async check fns and have **no**
  factored-out pure classifier — the standing gap that P3 (`.agent/plans/P3-check-trustworthiness.md`)
  closes.

- **Preflight-first, observability-as-a-property.** Validating a rendered config against the real image
  in ~1s (before a ~10-min devnet spend) is the single biggest agent-friendliness win; auto-triage on
  any failure means a run emits its own root cause. Both are normal outputs of a normal run, one source
  of truth (`--log-format json` for agents, pretty for humans) — never a separate agent-only tool
  surface. See DESIGN's thesis + Laws 1 and 5.

---

## See also

- `docs/DESIGN.md` — the mission and the design laws.
- `docs/CHECKS.md` — the per-check catalog (tiers, thresholds, feature-assertion status).
- `docs/local-kurtosis-e2e.md` — the runbook + the paid-for incident behind half the design.
- `.agent/plans/{P1-sim-preflight-triage,P2-consolidate-config-gen,P3-check-trustworthiness}.md` — the
  grilled rationale for each slice (internal).
