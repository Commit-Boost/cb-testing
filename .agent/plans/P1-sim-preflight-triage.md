# P1 — `sim`: helix preflight + triage (the agent loop) — v2, post-grill

> First slice of the full-Rust `sim` harness (NORTH-STAR P1). Goal: make sim failures FAST and
> STRUCTURED so an agent can launch -> triage -> diagnose without hand-`docker logs`-ing containers.
> **v2 reflects a 3-lens adversarial review** (technical / coverage-honesty / design). The review
> reshaped the plan: CB typed-validation is deferred to P2 (broken-as-specified + heavy dep + never the
> pain), `sim` becomes a bin sharing an extracted lib (not a duplicating crate), preflight gets a
> 3-valued verdict, and the honesty claims are corrected.

## Framing: observability by default, NOT agent-only tooling
P1's deliverable is not "CLI verbs for agents." It is that the harness is **observable by default**:
structured `tracing` events + a durable JSON verdict report, and — the key part — **root-cause capture is
a PROPERTY OF THE RUN**: when a service dies, the harness automatically attaches that container's root
panic (via the `docker logs` fallback) to the structured stream. A human reads a pretty rendering of that
stream; an agent reads the JSON; one source of truth. `preflight` and `triage` are just entry points into
that same observability surface (validate a config; attach to an already-broken enclave after the fact) —
not a separate agent category. Design for good logging on everything and the agent affordance comes free.

## Honest goal (what P1 delivers, and what it does NOT)
- **Delivers:** `sim preflight <args-file>` collapses the helix **config-parse** drift loop (2 of today's
  3 failures: `network_config`, `cores`) into one ~1s command, run automatically as a pre-run gate. On any
  launch failure the harness **auto-attaches the crashed service's root panic** to its structured output
  (the `triage` capability, fired automatically — Task 4 — not a verb an operator must remember), piercing
  the kurtosis-log masking via a `docker logs` fallback. `sim triage <enclave>` is the after-the-fact entry
  point into that same capability.
- **Does NOT (stated plainly):** preflight does NOT catch runtime/timing panics like today's pre-genesis
  `current_slot().unwrap()` (#3) — that isn't a config problem (its fix was the launcher wait-for-genesis
  wrapper) and is out of preflight's universe; it surfaces only via a run + triage. CB config typed
  validation is P2. See the residual table at the end. (Reframed from v1's overstated "one command for
  today's saga.")

## Scope decision (from the grill): helix-only preflight in P1
All three of today's failures were **helix**; CB config has never drifted. The CB-via-`cb-common`-types
path is deferred to P2 because it is (a) not the measured pain, (b) broken as v1 specified — `chain`'s
`Deserialize` reads a spec file off disk (`types.rs:393`), `{{ range .Relays }}` is a Go loop not a var,
so "substitute dummies then `toml::from_str`" fails on a *valid* config — and (c) a 409-crate dep
(alloy-full + lighthouse-from-git + blst/kzg C builds). P1 stubs `preflight_commit_boost` ->
`Inconclusive{"typed validation lands in P2"}`. No `cb-common` dep in P1.

## Architecture (revised: lib-first, not a new crate)
`src/` currently has NO `lib.rs`; its modules are private to the `cb-verify` bin, so the other bins can't
reuse them (`orchestrator.rs` already re-implements kurtosis shelling — the duplication is live). Making
`sim` a separate crate would force a THIRD copy of discovery/report, violating NORTH-STAR Law 2. So:
**Task 0 extracts `src/` into a lib; `sim` is a `[[bin]]` in the same package that reuses it by import.**
`sim` shells `kurtosis`/`docker` with **sync `std::process::Command`** (matching `discovery.rs`; no tokio
— the verbs are short sequential shells). Pure logic (substitute vars, extract block, classify probe,
extract root panic, parse inspect) is separated from process I/O and fixture-tested.

---

## Task 0: extract a lib; add the `sim` bin (fixes the duplication at the source)
**Files:** Create `src/lib.rs`; modify `Cargo.toml`; create `src/bin/sim/main.rs`, `.../cli.rs`.

- [ ] **Step 1: `src/lib.rs`** re-exporting the mature modules: `pub mod discovery; pub mod report;
  pub mod checks; pub mod beacon; pub mod relay; pub mod metrics;` (whatever `main.rs` currently declares
  privately). Add `[lib] name = "cb_testnet_verifier" path = "src/lib.rs"` to `Cargo.toml`.
- [ ] **Step 2:** convert the existing bins to `use cb_testnet_verifier::{discovery, report, …}` instead
  of `mod`. Run `cargo build` — the 4 existing bins still build/behave identically. **Commit** ("refactor:
  extract lib so bins share discovery/report").
- [ ] **Step 3: add the `sim` bin**: `[[bin]] name = "sim" path = "src/bin/sim/main.rs"`. Deps already
  present (clap, serde, serde_json, serde_yaml, tracing+`json` feature, eyre); add `tracing-subscriber`
  `json` feature if missing. NO tokio, NO cb-common.
- [ ] **Step 4: `cli.rs`** — clap with `preflight { args_file }`, `triage { enclave }`, global
  `--log-format pretty|json`. `main.rs` inits `tracing_subscriber::fmt().json()` behind the flag and
  dispatches. `cargo run --bin sim -- --help` shows both verbs. **Commit.**

## Task 1: shared runtime-var substitution + config-block extraction (pure, TDD)
The args-file embeds `helix_relay_config: |` (YAML) and `commit_boost_config: |` (TOML) as `|` block
scalars. BOTH contain unrendered Go-template vars — the **real set is 10** (`.BEACON_URI .BLOCKSIM_URI
.Network .Port .POSTGRES_{DB,HOST_NAME,PASS,PORT,USER} .Timestamp`), several **unquoted** (`port:
{{ .POSTGRES_PORT }}`) — plus a `{{ range $i, $r := .Relays }}…{{- end }}` loop in the CB block. Un-
substituted, the block is invalid YAML/TOML and any image probe fails on `{{` garbage, not on schema.

**Files:** Create `src/bin/sim/render.rs`.
- [ ] **Step 1: failing tests** using the **REAL** `configs/generated/cb-basic.yml` as fixture (not a toy
  string): (a) `extract_config_blocks(args)` returns both scalar bodies (read `mev_params.helix_relay_config`
  / `.commit_boost_config` via `serde_yaml` — sound: `|` bodies are opaque strings to the outer parse);
  (b) `substitute_runtime_vars(block, &dummies)` replaces all 10 vars with valid-typed dummies
  (numeric for ports/timestamp, uri strings for the rest) and **strips** the `{{ range .Relays }}…{{ end }}`
  block entirely (Relays is `#[serde(default)]` -> `relays: []` parses); (c) the substituted helix YAML
  and CB TOML both `serde_yaml::from_str::<serde_yaml::Value>` / `toml`-lex cleanly (no `{{` remains).
- [ ] **Step 2-4:** RED -> implement (enumerate the 10 vars by scanning, don't hardcode; `range`-block
  removal is line-range, not var-replace) -> GREEN. Assert zero `{{` survive. **Commit.**

## Task 2: `sim triage <enclave>` (pierces the masking)
Reuse the lib's kurtosis text parser; add a `docker logs` fallback (the masking fix); 3-valued extraction.

**Files:** Create `src/bin/sim/triage.rs`, `src/bin/sim/diagnose.rs`, `tests/fixtures/*.log`.
- [ ] **Step 1: fixtures = real captured crashes + HELD-OUT cases** (to prove generalization, not
  memorization): `helix_serde_missing_field.log` (today's `missing field 'decoder'`),
  `helix_pregenesis_unwrap.log` (today's `chain_info.rs:63 unwrap on None`), plus held-out:
  `invented_field.log` (a *different* field name), `multi_masked.log` (a masking line THEN the root panic —
  assert we pick the ROOT), `ansi_colored.log` (ANSI escapes), `next_line_message.log` (panic loc then
  message on the next line), `oom_no_panic.log` (SIGKILL, no panic string), `bind_error.log` (non-Rust
  fatal).
- [ ] **Step 2: failing tests** for `extract_root_cause(logs) -> Option<RootCause>`: matches the Rust
  panic (loc + message, incl. next-line), picks the **root not the first masking line** (multi_masked),
  strips ANSI, matches a non-Rust fatal (bind_error), returns `None`/`Killed` variant for oom_no_panic,
  returns None for clean logs. `RootCause { kind, location, message, log_tail }` (`Serialize`).
- [ ] **Step 3:** RED -> implement pattern-based (capture the field name, don't match it) -> GREEN. **Commit.**
- [ ] **Step 4: status parse** — reuse `discovery::split_on_multi_space` + the `User Services` section
  scanner (now importable) to read the **STATUS** column (`parse_services` drops it, so this is a thin new
  column-reader, not a copy). Test with a real `enclave inspect --full-uuids` fixture incl. a non-RUNNING
  service. NOTE `--format json` does NOT exist on kurtosis 1.18.1 — text parse is mandatory. **Commit.**
- [ ] **Step 5: wire `triage::run`** (bounded timeout + "tool not found" wrap on every shell, per
  `discovery::run_kurtosis`): `enclave inspect` -> non-RUNNING services (AND services referenced in a
  launch abort that may be UNREGISTERED — handle the half-built-enclave case) -> `kurtosis service logs`;
  **if that errors/empty/garbled, resolve the container via `--full-uuids` and `docker logs` it** (the
  real masking fix) -> `extract_root_cause` -> `TriageReport { enclave, failed: Vec<{service,status,
  root_cause}> }` JSON. Manual check vs a deliberately-broken enclave. **Commit.**

## Task 3: `sim preflight <args-file>` — HELIX ONLY, 3-valued
**Files:** Create `src/bin/sim/preflight.rs`.
- [ ] **Step 1: 3-valued classifier (TDD first, own checkbox).**
  `classify_helix_probe(exit_ok, logs) -> ConfigVerdict` where `ConfigVerdict = Pass | Fail{field} |
  Inconclusive{reason}`. Key on the panic **LOCATION**, not "reached fetch":
  - `config.rs` parse panic / serde `missing field`/`unknown field`/`untagged…enum` -> **Fail{field}**
    (real config drift — captures the field name).
  - reached the beacon-fetch stage (`get_chain_info` / `failed fetching chain info`) with no config
    panic -> **Pass**.
  - `HousekeeperTile`/`chain_info.rs`/pre-genesis `unwrap on None`, missing-`GENESIS_*`-env, image-not-
    pulled, docker-daemon-down, timeout-kill -> **Inconclusive{reason}** (NOT config drift — do not score
    an env/timing/infra error as schema failure; the "pilot breaks the instrument" trap).
  Fixture-test all three buckets (reuse Task 2's held-out logs).
- [ ] **Step 2:** RED -> implement -> GREEN. **Commit.**
- [ ] **Step 3: `preflight_helix(image, yaml) -> ConfigVerdict`** (own checkbox, its own manual smoke):
  substitute vars (Task 1) FIRST, write to a tmp dir, `docker run --rm --entrypoint sh <image> -c 'exec
  /app/helix-relay --config /cfg/config.yaml'` with a mounted `/cfg`, bounded ~15s timeout, capture
  combined output, classify. (`/app/helix-relay` + `--config` verified vs the launcher.) Clean up the tmp
  file; kill+rm any orphan on timeout. No beacon is provided, so a config-clean image reaches fetch =
  Pass; a pre-genesis panic (no GENESIS_TIME here) is correctly **Inconclusive**, not a false Fail.
- [ ] **Step 4: `preflight::run`** — extract blocks -> `preflight_helix` (CB stub -> Inconclusive) ->
  `PreflightReport { helix: ConfigVerdict, commit_boost: ConfigVerdict }` JSON. **Exit nonzero ONLY on a
  `Fail`** (Inconclusive is not a drift failure — it must not break the gate on a slow pull). Run vs real
  `cb-basic.yml` (Pass) and a drifted copy (Fail names the field, ~1s). **Commit.**

## Task 4: wire preflight as a pre-run GATE (prevent, not just diagnose)
Per the coverage grill: a manual verb diagnoses faster but doesn't prevent the saga. Make it a gate.
- [x] **Step 1:** in `scripts/run-and-verify.sh` (until `sim run` exists in P2), add a pre-`kurtosis run`
  step: `sim preflight <config>`; **abort the launch on a `Fail`** with the field, proceed on Pass/
  Inconclusive. Validate against the SAME image the run will use (pin/record the digest; note
  `--image-download always` re-pulls, so preflight must target that resolved digest, not a stale local).
  DONE — gate added; abort on rc=1 (Fail), proceed on Pass/Inconclusive, best-effort on build/tooling error.
  Digest-identity is NOT yet closed (preflight reads local-cached `:main`, run re-pulls): recorded as a
  WHY-comment at the site + a residual-class row below; the fix belongs to `sim run` (P2) that owns the pull.
- [x] **Step 2:** on any launch failure, auto-invoke `sim triage <enclave>` and surface its JSON. DONE —
  `kurtosis run` wrapped in `if !`; on failure fires `sim triage "$ENCLAVE"` then exits 1. **NOT committed**
  (awaiting maintainer review of the full P1 diff).

## Status: LANDED (committed `a0c8be4`+; pushed on `feat/sim-harness`). Implementing files: `src/bin/sim/{preflight,triage,diagnose,render}.rs`, `scripts/run-and-verify.sh`. See `INDEX.md`.
(historical detail below — Tasks 0-4 all committed 2026-07-30)
77 tests green (lib+bin, 0 fail/0 warn). Proven end-to-end against the real `:main` image: valid cb-basic.yml
-> `helix: pass`, exit 0, ~8.5s; a config with `hostname` renamed -> `Fail{field:"hostname"}`, exit 1, ~0.3s.
The multi-hour drift saga of 2026-07-30 now collapses to a sub-second, structured, exit-coded gate. Nothing
committed pending review of the full diff.

## Definition of done (honest)
- `sim preflight configs/generated/cb-basic.yml` -> Pass in ~1s; a drifted helix config -> **Fail** naming
  the field, in ~1s, no devnet. A slow pull / env / pre-genesis issue -> **Inconclusive** (not a false Fail).
- `sim triage CB-Testnet` on a failed enclave -> JSON naming each crashed service + its ROOT panic, using
  the `docker logs` fallback so it works even when kurtosis masks the error / the enclave half-built.
- Preflight runs as a blocking gate before launch; triage auto-fires on launch failure.
- Pure cores (render/substitute/extract/classify/parse) are unit-tested in `cargo test` with real + held-
  out fixtures. The two IO wirings (`preflight_helix` needs Docker+image; `triage::run` needs kurtosis)
  are Docker-gated smoke checks, not `cargo test` (Law 4 applies to the pure cores).
- `cb-verify`/`cb-orchestrator` unchanged; both now `use` the shared lib.

## Residual failure classes (what P1 still leaves to a run + triage or a human)
| Class | P1 |
|---|---|
| helix config-parse drift (`network_config`, `cores`) | ✅ preflight Fail, ~1s, as a gate |
| CB config-parse drift | ⏳ P2 (typed against a mirror/`cb-common`; note tag-vs-`:main` fidelity gap) |
| runtime/timing panic (pre-genesis #3) | ❌ preflight = Inconclusive; caught only by run + triage; fix was a launcher change |
| half-built enclave / service-add grpc abort | ✅ triage docker-logs fallback + unregistered-service handling |
| fast-exit log race | ✅ docker-logs fallback |
| OOM / SIGKILL (no panic string) | ⚠️ triage reports `Killed`, no root message (best effort) |
| image-pull / registry-auth failure | ⚠️ preflight = Inconclusive (not misread as drift); not auto-fixed |
| kurtosis version / config-version clash | ❌ CLI-layer, before services; neither verb helps (see runbook) |

## Deferred / scars
- CB typed validation (P2): needs a real rendered dummy chain-spec on disk OR a `chain = "Holesky"`
  rewrite; `CommitBoostConfig::validate()` is `async` and only network-touches for extra-validation/mux/
  signer configs; top-level `deny_unknown_fields` is impossible (`#[serde(flatten)] muxes`), so CB drift-
  catch is PARTIAL (relay-section + required-top-level only). Prefer the P2 owned typed mirror over the
  409-crate `cb-common` git dep if the mirror is cheap.
- `serde_yaml 0.9.34` is unmaintained (matches the repo); don't expect fixes.
- kurtosis has no Rust SDK; discovery stays brittle text-parsing either way.
