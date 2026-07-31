# P2 — consolidate config generation into `sim` (retire the Python generator)

> **For agentic workers:** implement task-by-task with TDD (RED test first, then GREEN), a review
> subagent between tasks, and a commit per slice. Steps use `- [ ]` checkboxes.

**Goal:** Move Kurtosis config generation out of `scripts/generate_kurtosis_configs.py` into the `sim`
Rust binary (the ratified full-Rust consolidation), and collapse the image-name drift — WITHOUT typed
serde mirrors of the config bodies.

## Why this is NOT a typed-mirror plan (a prior draft was; three grills killed it)
The earlier draft proposed typed `HelixRelayConfig` / commit-boost serde structs. Three adversarial
reviews + a direct diff refuted its premise:
- **The "6 duplicated ~90-line templates" claim is false.** The Python is already DRY: helix is ONE
  function whose emitted block is **byte-identical across all 6 scenarios** (verified by diff — zero
  variation); CB is ONE parameterized function (`build_cb_toml_basic`) with ≤7 changed lines per scenario
  (mux's 285-line diff is pubkey *data*, not template structure). There is no duplication for typing to kill.
- **Typed helix buys no guard.** Helix types are not importable (divergent org/branch), so a mirror is NOT
  checked against helix — it's hand-guessed from binary panics either way (North Star Law 1 caveat says so
  outright). Preflight stays the only reality check, exactly as with a string. Typing also thins the ~40
  lines of hard-won drift comments (the `network_config` removal, the binary-verified 10-field `CoresConfig`)
  that ARE the expensive knowledge.
- **The serde mechanism is fragile.** serde_yaml has no unquoted-scalar emit hook, so runtime holes like
  `port: {{ .POSTGRES_PORT }}` (unquoted) force a "serialize a sentinel number, then string-replace it"
  dance. Sentinels collide: `2000` collides with `late_in_slot_time_ms = 2000`; `db_name` and `user` are
  both `"helix"` so one value can't map to two placeholders; `instance_id` contains the substring `helix`.
  All silent-corruption traps — introduced solely by choosing serde structs.
- **"Just emit concrete values, no templating" is refuted by the code.** `run-and-verify.sh` feeds the file
  straight to `kurtosis run --args-file`; the ethereum-package fork is the template renderer —
  `mev_boost_launcher.star` and `helix_relay_launcher.star` call `plan.render_templates(..., {Timestamp,
  Port, Relays, POSTGRES_HOST_NAME, BEACON_URI, BLOCKSIM_URI, ...})` with **runtime service-discovery
  values that do not exist until kurtosis is running.** The `{{ }}` MUST survive into the checked-in file.

**Conclusion:** port the templates verbatim into Rust `const` strings (the `{{ }}` runtime holes stay as
literal text — no serialization, so the entire sentinel/quoting hazard evaporates), and put the typing only
where it pays: a `Scenario` enum + one `Images` map for assembly. This satisfies Law 1 ("Python dies, one
source") and Law 2 ("one image map") and the ratified full-Rust direction, at a fraction of the code and
none of the fragility.

## The one real bug this fixes
The Python's `MEV_BOOST_IMAGE` DEFAULT is `commit-boost/pbs:kurtosis` (`generate_kurtosis_configs.py:50`),
but the correct image is `commit-boost/commit-boost:kurtosis` (`docs/local-kurtosis-e2e.md:43` "NOT `pbs:*`";
`.env.example` uses `commit-boost/commit-boost:latest`). Today the green run only works because a `.env`
override masks the wrong default. The unified `Images` map bakes the correct value as the default.

## Mechanism (what the code does)
- **helix block:** one `pub const HELIX_RELAY_CONFIG: &str` — a verbatim port of the Python
  `build_helix_relay_config()` output, **comments preserved**. Contains literal `{{ .POSTGRES_* }}`,
  `{{ .BEACON_URI }}`, `{{ .BLOCKSIM_URI }}`. No variation across scenarios (proven), so it's a plain const.
- **CB block:** a `cb_toml(params) -> String` that reproduces `build_cb_toml_basic` — a base template with
  Rust-side injection of the generate-time knobs (timeouts, `extra_pbs_lines`, `per_relay_lines`) and the
  literal `chain = { genesis_time_secs = {{ .Timestamp }}, path = "{{ .Network }}" }`, `port = {{ .Port }}`,
  and `{{ range $index, $relay := .Relays }} … {{- end }}` loop as literal text. A separate `cb_toml_mux`
  reproduces `build_cb_toml_mux` (two `[[mux]]` blocks + per-node `validator_pubkeys` lists loaded from
  `keys/node-{0,1}-pubkeys.json`). Generate-time injection is plain string building — NOT serde — so no
  quoting/sentinel issues.
- **assembly:** a `Scenario` enum (the 6 variants) with `fn args_file(&self, images: &Images) -> String`
  that joins the shared static fragments (`participants`, `additional_services`, `network_params` — kept as
  vetted `const` strings; they never drifted and are out of Law 1's scope) + `mev_type: custom` + the
  `mev_params` wrapper embedding the helix const and the CB string as the two `|` block scalars.
- **`Images` struct:** the ONE image map (helix/mev_relay/mev_boost/builder_el/builder_cl), correct defaults,
  `.env` overrides applied at the CLI boundary (`generate::run`), NOT inside the pure assembly.

## Acceptance (simple + strong): byte-identity to the golden
Because this is a verbatim port, `sim generate <scenario>` output is **byte-identical** to the Python
golden (modulo image values, which we bake). So the oracle is a plain byte-diff — stronger and far simpler
than a semantic comparator. Golden set (all 6) is snapshotted at `scratchpad/p2-golden/*.yml`; Task 0
copies them into `tests/fixtures/golden-configs/` with image values normalized to the baked defaults so the
test is hermetic (does NOT depend on the ambient `.env`). Plus: `sim generate cb-basic` output still passes
`sim preflight` (helix arm) against the real image — the P1 gate, unchanged.

## Scenarios (6) — what varies (measured, not assumed)
| scenario | relays | CB delta vs basic | helix |
|---|---|---|---|
| cb-basic | helix | — | identical |
| cb-multiple-relays | helix, flashbots | 0 lines (+`mev_relay_image`) | identical |
| cb-skip-sigverify | helix | +1: `skip_sigverify = true` | identical |
| cb-extra-validation | helix | +2: `extra_validation_enabled` + `rpc_url` | identical |
| cb-timing-games | helix, flashbots | timeouts 400/2000; +3 per-relay lines INSIDE the range loop | identical |
| cb-mux | helix, flashbots | two `[[mux]]` blocks, 256 keys/node from JSON | identical |
Note: the helix block is byte-identical in every scenario (do not add per-scenario helix logic).

---

## Task 0: golden fixtures (hermetic) + the byte-diff harness
**Files:** Create `tests/fixtures/golden-configs/*.yml`; new module `src/bin/sim/genmodel/mod.rs`.

- [ ] **Step 1:** copy the 6 goldens from `scratchpad/p2-golden/` into `tests/fixtures/golden-configs/`.
  Normalize the 5 image values in each to the baked defaults (`helix-relay:kurtosis`,
  `commit-boost/commit-boost:kurtosis`, etc.) so the fixture is hermetic — it must not encode the box's
  current `.env`. (The generator's OWN default emission, once corrected, must match these.)
- [ ] **Step 2:** add `mod genmodel;` to `src/bin/sim/main.rs`; add a `pub fn assert_matches_golden(scenario:
  &str, produced: &str)` test helper in `genmodel/mod.rs` that byte-diffs and, on mismatch, prints the first
  differing line with context. Trivial unit test: a golden matches itself; a golden with one line flipped
  does not (naming the line). Run `cargo test --bin sim` — RED (module empty) then GREEN.
- [ ] **Step 3: commit** — `test(sim): hermetic golden config fixtures + byte-diff harness`.

## Task 1: port the generator into `sim genmodel` + `sim generate`
**Files:** Create `src/bin/sim/genmodel/{helix.rs,cb.rs,scenario.rs}`; `src/bin/sim/generate.rs`; extend
`cli.rs` with `Generate { scenario: Option<String>, out_dir: PathBuf }`.

- [ ] **Step 1 (test-first, helix+cb bodies):** port `HELIX_RELAY_CONFIG` const (verbatim, comments kept)
  into `helix.rs`; port `cb_toml(params)` + `cb_toml_mux(...)` into `cb.rs`. RED tests: the helix const
  equals the helix block of `golden-configs/cb-basic.yml` (extracted); `cb_toml(basic params)` equals the
  CB block of cb-basic; `cb_toml(timing params)` equals cb-timing-games' CB block (exercises per-relay
  lines inside the loop); `cb_toml_mux(2 keys/node)` produces the right STRUCTURE (two `[[mux]]`, per-node
  `validator_pubkeys`, `[[mux.relays]]`) — use 2 synthetic keys/node in the UNIT test, NOT 256.
- [ ] **Step 2:** implement to GREEN.
- [ ] **Step 3 (assembly):** `scenario.rs` — `Images` struct (baked defaults) + `Scenario` enum +
  `args_file`. RED test: for EACH of the 6 scenarios, `Scenario::X.args_file(&Images::defaults())`
  byte-equals its golden (the 256-key mux path is exercised here at full size, loading the real
  `keys/*.json`, so the full mux golden is the characterization oracle — but the cb.rs UNIT test above
  stays at 2 keys). GREEN.
- [ ] **Step 4:** `generate::run` writes `configs/generated/<scenario>.yml` (one or all); apply `.env` image
  overrides here (at the IO boundary). Smoke: `sim generate --out-dir /tmp/gen`, then
  `sim preflight /tmp/gen/cb-basic.yml` → helix Pass. **Commit** — `feat(sim): sim generate — port config
  generation into Rust with a unified image map`.

## Task 2: retire the Python generator + stale example; repoint callers
- [ ] **Step 1:** delete `scripts/generate_kurtosis_configs.py` and `configs/example-kurtosis-config.yml`.
- [ ] **Step 2:** repoint `justfile` `generate-configs:` → `cargo run --bin sim -- generate`. Update the
  doc refs from the Python script to `just generate-configs` / `sim generate`: `README.md` (lines ~33, 48,
  50, 53, 71, 201, 204), `docs/local-kurtosis-e2e.md` (54, 112). Fix the `commit-boost/pbs` mention in
  `README.md:29` to the correct image.
- [ ] **Step 3:** a `#[test]` (or a `just` check) that shells `sim generate` for all 6 into a tempdir and
  byte-matches the golden fixtures — the end-to-end characterization proving the port is faithful. **Commit**
  — `refactor(sim): retire the Python config generator; sim generate is the one source`.

---

## Definition of done
- `sim generate` reproduces all 6 scenarios byte-for-byte vs the golden fixtures; `cb-basic` passes
  `sim preflight` (helix arm) against the real image.
- The Python generator + stale example are gone; `just generate-configs` runs `sim generate`.
- One `Images` map with the CORRECT `commit-boost/commit-boost` default (the live `pbs` bug fixed).
- Everything under `-D warnings`; new code TDD'd against golden fixtures.

## Explicitly NOT doing (and why)
- **Typed serde config mirrors** — refuted above (no duplication to kill, no guard gained, fragile). If CB
  config ever actually drifts (it hasn't; helix is the drifter), revisit — but only behind a real CB-image
  preflight, not golden-equivalence.
- **CB-image preflight** (fill P1's `Inconclusive` stub) — needs the real CB image to parse the TOML, whose
  `chain` deserialize reads a spec file off disk (needs a mounted dummy spec). Its own slice (P2.5). Until
  then CB parity rests on byte-identity to the known-good golden. Also note: mux's `{{ index .Relays N }}`
  has no dummy in `render.rs`, so the mux CB block is not preflightable today (document, don't paper over).

## Flagged for J (higher-value-than-typing, per the scope grill — do NOT start without a nod)
The scope grill argued P3's false greens outrank config-gen ergonomics: the mux pass-gate keys on
`total_events` not `pubkeys_verified` (reports "all routing verified" having verified zero decisions when CB
debug logging is off), and the best-bid check unions-by-slot instead of comparing bid VALUES across relays
(one delivering relay passes identically to genuine two-relay aggregation). A harness that lies green is
worse than an ugly generator. These are Law 3 defects (`src/checks/`). Recommend J weigh pulling them ahead
of any further config work. Left as a flag because they change VERIFICATION verdicts — a judgment call worth
J's eyes, not an autonomous bake-in.
