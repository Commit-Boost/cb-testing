# cb-testing - agent orientation

An **opinionated block-building simulation substrate for Commit-Boost**. It stands up a real Ethereum
devnet (helix relay + reth-rbuilder builder + the CB sidecar) via Kurtosis, exercises one PBS feature end
to end, and returns a **trustworthy verdict**. The point is not "the devnet booted"; it is a verdict you
can gate a release on. Nothing here is a mock.

Rust workspace (one lib, three bins):

| Target | Path | Role |
|---|---|---|
| `cb_testnet_verifier` (lib) | `src/lib.rs` | shared: `beacon`, `relay`, `metrics`, `discovery`, `checks/*`, `report` |
| `cb-verify` (bin, default-run) | `src/main.rs` | the verifier: discover -> observe -> run checks -> report -> exit code |
| `sim` (bin) | `src/bin/sim/main.rs` | `generate` \| `preflight` \| `triage` \| `checks` \| `doctor` \| `diff` |
| `cb-orchestrator` (bin) | `src/orchestrator.rs` | parallel multi-enclave runner (`just test-all`) |

## Authoritative docs (read these first, this file is the ROUTER)
- **docs/NORTH-STAR.md** - WHY the repo exists + **the 7 design laws** (each prevents a named smell).
  Read when a change feels like it is fighting the grain. Law 1 real-schema configs, Law 3 every scenario
  asserts its feature FIRED, Law 4 verdict logic is TDD-able without a devnet, Law 5 observability is a
  system property (never build an agent-only tool surface), Law 7 coverage is a matrix (EL/CL pairs).
- **docs/ARCH.md** - HOW it fits: end-to-end flow, module map, the config <-> fork seam, the verdict model.
- **docs/CHECKS.md** - the authoritative **per-check contract** (tier, source, pass/warn/fail/skip conditions).
- **docs/DEVELOPING.md** - the dev loop + how to add a check / a scenario.
- **docs/SWEEP-BACKLOG.md** - the **live** queue AND the findings log (every live-devnet result gets recorded
  there). Highest-density source of hard-won facts; read the tail before starting anything.
- **docs/fork-delta.md** - what our `ethereum-package` fork changes vs upstream, file by file.
- **docs/local-kurtosis-e2e.md** - the operational runbook + the paid-for incidents behind half the design.
- **docs/plans/INDEX.md** - status of every plan (`live` steers, `landed` is history).
- **README.md** - user-facing quick start (user-facing quick start).

## Runtime facts
- **Rust 1.91+, edition 2024.** No Node, no Python. `cargo test` is pure, hermetic, seconds (no docker).
- **Kurtosis CLI pinned to 1.18.1.** The parsers read its human text tables (1.18.1 has no `--format json`
  for them) and a newer CLI writes a `config-version: 9` `~/.config/kurtosis/kurtosis-config.yml` that
  1.18.1 cannot read. `sim doctor` checks this for you.
- **No Kurtosis Rust SDK exists.** All enclave ops are `std::process::Command` + text parsing
  (`discovery.rs`, `triage.rs`). Inherent, not a rewrite candidate.
- `sim` is deliberately **sync / no tokio** (it shells out and blocks); `cb-verify` and `cb-orchestrator`
  are tokio (concurrent HTTP polling / parallel enclaves).
- **The CB sidecar image is BUILT, not pulled** (`just build-cb-image` -> `commit-boost/commit-boost:kurtosis`
  from the sibling `../commit-boost-client`). helix / reth-rbuilder / lighthouse are public pulls.

## USING it

`sim` is a workspace bin, not a PATH command: invoke it as `cargo run --quiet --bin sim -- <verb>` or, after
`cargo build --release`, as `./target/release/sim <verb>`. Written `sim <verb>` below for brevity.

```bash
sim doctor                        # host preflight: kurtosis 1.18.1, docker, memory, CB image, submodule
git submodule update --init       # the forked ethereum-package (empty otherwise; load-bearing)
just build-cb-image               # once: builds commit-boost/commit-boost:kurtosis from ../commit-boost-client
just e2e                                            # cb-basic, end to end
just e2e configs/generated/cb-mux.yml               # any scenario
```

`just e2e` = `generate-configs` + `pull-images` + `just testnet <config>`, which calls the launcher:

```bash
./scripts/run-and-verify.sh --config configs/generated/cb-basic.yml \
    --json --live-metrics --min-epochs 1 --target-epoch 1 --keep --skip-finalization -v
```
Launcher flags (all real, `scripts/run-and-verify.sh`): `--config --enclave --package --keep --json
--json-dir DIR --strict --live-metrics --skip-finalization --timeout --min-epochs --target-epoch -v`.
It (0) pre-builds `cb-verify` OFF the critical path, (1) removes the stale enclave, (1b) runs
`sim preflight` as a gate, (1c) warns on low host memory (`LOW_MEM_ABORT=1` to abort instead),
(2) `kurtosis run` (auto-fires `sim triage` on launch failure), (3) runs the pre-built `cb-verify`.
With `--json` and no `--json-dir` it saves the report to `<repo>/<enclave>.json`.

`cb-verify` direct (`src/main.rs`): `--enclave --config --min-epochs --target-epoch --timeout
--mev-threshold --json --verbose/-v --strict --live-metrics --show-logs --skip-finalization-check
--output-dir`. Note the launcher's flag is `--skip-finalization`, the binary's is `--skip-finalization-check`.

Against an already-running enclave (no observation window): `just verify-now <enclave>`,
`just verify-with-config <config> <enclave>`, `just test-mux <enclave> <config>`, `just show-logs <enclave>`.

### Scenarios and config generation
`configs/generated/*.yml` are **gitignored build products** of the typed generator; the tracked truth is
`tests/fixtures/golden-configs/`. Nine scenarios (`Scenario::ALL`, `src/bin/sim/genmodel/scenario.rs`):
`cb-basic`, `cb-basic-nethermind-prysm` (Law 7 alt EL/CL pair), `cb-multiple-relays` (two helix instances,
divergent per-relay subsidies), `cb-skip-sigverify`, `cb-sigverify-diff` + `cb-sigverify-diff-control`
(a real ON/OFF differential), `cb-timing-games`, `cb-extra-validation`, `cb-mux` (256 validators split
across two relays).

```bash
sim generate                      # all nine -> configs/generated/   (= just generate-configs)
sim generate cb-mux --out-dir /tmp/x
sim generate --check              # drift gate: nonzero if on-disk configs != what the generator emits
sim preflight configs/generated/cb-mux.yml   # ~1s: parse the rendered config with the REAL helix image
sim checks --list [--json]        # the check contract, machine-readable
sim diff a.json b.json [--json]   # verdict/provenance regression gate between two reports
sim triage <enclave>              # each dead service's ROOT panic, as JSON
sim --log-format json <cmd>       # structured event stream for agents (default: pretty)
```
`.env` (gitignored, see `.env.example`) overrides the embedded docker images and is read only at the
`sim generate` CLI boundary, so assembly stays pure.

## THE VERDICT MODEL (load-bearing)
A run emits a `VerificationReport` (`src/report.rs`) plus an exit code. Each `CheckResult` has `id`,
`tier` (1/2/3), `result` (`PASS`/`FAIL`/`WARN`/`SKIP`), `detail`, `data`.

**The exit code keys ONLY on a tier-1 FAIL** (`report::exit_code`): `2` = no tier-1 check ran at all
(setup/discovery failure), `1` = some tier-1 check FAILed, `0` otherwise. `WARN` and `SKIP` are
**non-fatal at every tier** and never move the exit code or the overall result.

**The consequence a consumer MUST internalize:** several checks that exist precisely to catch an anomaly
report it as `WARN` (relay equivocation in `payload_hash_match`, unverifiable routing in `mux.routing`,
a best-bid shortfall in `relay.best_bid`). A run that hits them **still exits 0**. Parse the JSON and
inspect each check's `result`; never gate on the exit code alone. `--strict` promotes selected soft
warnings to FAIL.

**Escalation:** `cb_metrics` checks whose id ends in `_matrix`, plus `cb_relay_v2_unsupported`, are
authored at tier 2 but **promoted to tier 1 when they FAIL** (`src/checks/cb_metrics.rs`), because a real
relay 5xx or a lost blinded-block submission is a genuine pipeline failure. So a matrix FAIL does gate
the exit code despite its nominal tier.

Per-check contract, data sources, and death-mode behavior: **docs/CHECKS.md**. `sim preflight` has its own
narrower 3-valued verdict (`Pass` / `Fail{field}` / `Inconclusive`) and only its `Fail` aborts a launch.

## DEBUGGING with it (the highest-value section)
**The method that has repeatedly paid off here: get the RAW evidence before believing a check's summary.**
Every misdiagnosis in this repo's history was a check's `detail` string read as fact. `data` is closer to
truth than `detail`; raw logs and raw counters are closer still.

```bash
sim doctor                                  # is it me or the box?
sim preflight <args-file>                   # ~1s real-image config parse, BEFORE a ~10min spend
sim triage <enclave>                        # broken enclave -> per-service root panic (skips masking lines,
                                            #   falls back to `docker logs` when kurtosis's broker masks it)
kurtosis enclave ls                         # ALWAYS check for a live run before touching anything
kurtosis enclave inspect <enclave>          # services, statuses, mapped ports
kurtosis service logs <enclave> <service> -n 200000 | sed 's/\x1b\[[0-9;]*m//g'
kurtosis port print <enclave> <service> <port-name>
```
Run with `--keep` (the `just testnet` default) so the enclave survives for inspection; tear down explicitly
with `kurtosis enclave rm -f <enclave>`.

- **Logs need ANSI stripping.** CB logs are colored; the parsers use `strip_ansi` internally, you need the
  `sed` above by hand. Services are `commit-boost-001`, `helix-relay-N`, `el-{i}-{el}-{cl}`,
  `cl-{i}-{cl}-{el}` (the EL/CL pair is in the name; see Law 7).
- **Scrape CB's Prometheus counters directly for the RAW per-code view.** The report BUCKETS status codes
  (2xx/4xx/5xx/timeout/transport), which hides which exact code fired - that bucketing is what produced a
  "47.5% relay 5xx" panic that was entirely CB's synthetic 555. Get the truth:
  ```bash
  kurtosis service exec <enclave> commit-boost-001 "curl -s http://localhost:9090/metrics" \
    | grep -E 'cb_pbs_relay_status_code_total|cb_pbs_beacon_node_status_code_total'
  ```
  Other counters worth reading raw: `pbs_submit_block_v2_unsupported_total`,
  `cb_pbs_submit_block_v2_fallback_to_v1_total`, `cb_pbs_relay_latency`.
- **Read the report JSON, not the terminal render**: `<enclave>.json` at the repo root after a `--json` run
  (e.g. `CB-Testnet.json`). Then `sim diff old.json new.json` for the delta.
- **Cross-check the two sides of a matrix.** `relay_side` vs `beacon_side` in a `cb_*_matrix` check's `data`
  is what distinguishes "the relay rejected us" from "the CL never asked": relay 4xx + beacon 5xx means CB
  forwarded and the relay refused, then returned 502 to the CL.
- Middle ground before a full run: `just verify-now`, `just test-mux`, `sim preflight`. **A ~10-minute e2e is
  the final confirmation, never the debugger.**

## Known traps (hard-won; do not re-derive)

**The signer runs as uid 10001 and the devnet's `secrets/` dir is mode 600 root-owned.** Kurtosis does
NOT chown files-artifacts on mount, so a non-root container cannot even traverse it, and CB's keystore
loaders skip unreadable entries with `warn!` rather than failing - producing a healthy signer holding
ZERO keys. Use the `teku-keys` + `teku-secrets` pair (755/777), which is also what the package's own
web3signer launcher relies on. Verified live; six package launchers force `User(uid=0)` for the same
reason.

**Never let a grep's SILENCE mean success.** `cargo test ... | grep "test result:"` prints nothing
when the build fails, which reads identically to "no output, fine". A broken test suite was committed
this way (2026-08-04). Gate on the EXIT CODE, not on matched lines:
```bash
set -o pipefail
cargo test --all-targets 2>&1 | grep -E "^test result:"; echo "TEST_EXIT=$?"
cargo clippy --all-targets -- -D warnings >/dev/null 2>&1; echo "CLIPPY_EXIT=$?"
cargo fmt --check >/dev/null 2>&1; echo "FMT_EXIT=$?"
```
This is the same defect class the harness keeps finding in itself: a check that cannot distinguish
"no signal" from "bad signal". Apply it to devnet runs too - an empty log tail is not a passing run.

- **CB's synthetic status codes 555 and 556 are NOT relay-served.** 555 = `TIMEOUT_ERROR_CODE`, CB's own
  client-side deadline cancellation; 556 = WS transport error. They bucket separately (`timeout`,
  `transport`) and are excluded from the 5xx denominator. Counting them as relay 5xx tier-1-FAILed a fully
  green timing-games run. High rates WARN, never FAIL, not even under `--strict`.
- **helix `router_config.enabled_routes` gates routes, and a missing route looks like a client bug.**
  `GetPayloadV2` was absent from our generated helix config, so helix 404'd `/eth/v2/builder/blinded_blocks`,
  CB (correctly) refused to downgrade to v1, and prysm's entire MEV path died. Hours were spent suspecting
  nethermind/prysm; it was OUR config. `cb_relay_v2_unsupported` now names it in one line.
- **A check's `detail` string can be flat wrong.** `cb_submit_blinded_block_matrix` reported "proposer never
  chose a builder block" while the relay had rejected 26 blinded blocks - the opposite of reality. Fixed, but
  the lesson generalizes: **trust `data`, verify against raw logs and counters.** A diagnosis naming the
  wrong component is worse than no diagnosis.
- **Kurtosis pinned at 1.18.1**; a newer CLI's `config-version: 9` config file breaks it (delete the file on
  downgrade, then `kurtosis analytics disable`).
- **Pass `kurtosis run` an ABSOLUTE package path from a script.** `run-and-verify.sh` resolves
  `$REPO_DIR/ethereum-package`; a relative `./ethereum-package` resolves against the caller's cwd and fails
  with a confusing "no kurtosis.yml" error.
- **`ethereum-package/` is a FORK** (`JasonVranek/ethereum-package`, branch `cb-testing`), pinned as a
  detached-HEAD submodule and load-bearing (empty without `--init`). Changes there must be **pushed** or
  every other clone breaks. There is no `upstream` remote configured. See docs/fork-delta.md.
- **`configs/generated/` is gitignored, but `src/bin/sim/render.rs` and `genmodel/scenario.rs`
  `include_str!` `configs/generated/cb-basic.yml` at COMPILE time** (a code comment even calls it "TRACKED";
  it is not). A fresh clone therefore cannot build `sim` until that file exists, and `sim` is the generator.
  Recovery: `cp tests/fixtures/golden-configs/cb-basic.yml configs/generated/` (verified byte-identical),
  then `just generate-configs`.
- **helix's config schema drifts with the moving `:main` tag.** The source of truth is the running binary's
  serde metadata, not any checked-in mirror. Reconcile by parsing against the real image (that is exactly
  what `sim preflight` is), never by editing to match a local helix checkout.
- **NEVER pattern-kill processes.** No `pkill -f` / `killall`: the pattern matches your own shell and you
  kill your session. Read the PID (`ps -eo pid,args | grep <pattern>`) and kill by explicit number.
- **A devnet may be live on this box.** `kurtosis enclave ls` first; never `kurtosis clean -a` or remove an
  enclave you did not create.

## DEVELOPING
**The gates, all four, before any commit** (mirror of CI plus the drift gate):
```bash
cargo test --all-targets && cargo clippy --all-targets -- -D warnings && cargo fmt --check
sim generate --check          # config-generation drift gate (NOT in CI today; run it yourself)
```
`just ci` = `check` + `test` + `lint`. Green `just ci` says nothing about the devnet path.

**Adding a check - the Law-4 pure-`classify_*` seam (non-negotiable).** Split every judgement into
(a) a **pure classifier** taking already-fetched data and returning a `CheckResult`, unit-tested on **both
sides of every boundary**, and (b) a **thin async fetch shell** holding zero verdict logic. Worked examples:
`cb_metrics::{collect_endpoint_stats, classify_endpoint}` (the cleanest seam in the repo),
`feature_fired::{classify_marker_feature, classify_skip_sigverify}`, `payload_matching::classify_payload_matches`,
`best_bid::classify_best_bid`, `mux_routing::classify_mux_routing`. Then wire the fetch fn into
`run_verification` in `src/main.rs`. The anti-pattern is welding the verdict into the `await` path:
`chain_health` and `relay_pipeline` still do this and are the standing gap. **Pick the tier deliberately**
(it is the severity contract) and prefer **WARN over a silent PASS** when the check could not actually
verify anything - that is how false-greens ship. Full recipe: docs/DEVELOPING.md §2.

**Adding a scenario:** add the variant to `Scenario` + `Scenario::ALL` in `src/bin/sim/genmodel/scenario.rs`,
fill `name()`/`comment()`/`relays()`/`cb_block()`/`network_params()`, build the CB TOML via `CbParams` +
`cb_toml` in `genmodel/cb.rs` (the helix block in `genmodel/helix.rs` is shared and byte-identical across
scenarios), run `just generate-configs`, then copy the output into `tests/fixtures/golden-configs/`.
**The golden-fixture byte-identity test is the oracle** (`every_scenario_matches_its_golden`): it proves the
generator reproduces its own output deterministically, NOT that the config is a working devnet - only a real
run proves that. And **Law 3: ship the scenario with a check that positively asserts its feature FIRED**
(see `src/checks/feature_fired.rs`), or it is a non-test. Full recipe: docs/DEVELOPING.md §3.

**Where things live:** `src/checks/` (the verdicts), `src/bin/sim/` (generate/preflight/triage/checks/
doctor/diff + `genmodel/`), `src/report.rs` (report shape + `exit_code`), `src/discovery.rs` (kurtosis text
parsing), `tests/fixtures/` (log fixtures + golden configs), `scripts/run-and-verify.sh` (the launcher).

## Documentation discipline (KEEP THIS FILE UPDATED)
**If you change behavior, flags, check ids/tiers/verdicts, scenario names, or the config-generation contract,
you MUST update this file AND its companion doc IN THE SAME COMMIT:**

| You changed | Also update, same commit |
|---|---|
| a check's id, tier, verdict conditions, or data source | `docs/CHECKS.md` **and** `src/bin/sim/checks_catalog.rs` |
| a CLI flag, a `just` target, the launcher | this file + `README.md` |
| a scenario, the generator, the helix/CB blocks | `tests/fixtures/golden-configs/` (regenerate) + this file's scenario list |
| a design law, a ratified direction | `docs/NORTH-STAR.md` |
| a live-devnet result or a new defect | `docs/SWEEP-BACKLOG.md` (findings log) |
| a plan shipped | `docs/plans/INDEX.md` (`live` -> `landed`) |

**`sim checks --list` is a hand-maintained static catalog with NO compiler enforcement** (`checks_catalog.rs`
says so at the top): the checks are constructed imperatively across several modules with no registry to
reflect on, so drift is caught only by review. This is not hypothetical - `cb_relay_v2_unsupported` shipped
in `src/checks/cb_metrics.rs` while missing from BOTH `checks_catalog.rs` and `docs/CHECKS.md`, in the very
commit that added it (caught on the next read, 2026-08-04, now synced). Adding a check is a THREE-file
change. If a real registry ever lands in `src/checks`, derive the catalog from it and delete this note.

**New hard-won gotchas belong in the "Known traps" section above**, with the evidence that bought them.
A trap that lives only in a commit message will be paid for twice.

## Coding preferences
No em dashes in prose, plain language, hypothesis-driven iteration, many small files. Pure cores with tests
on both sides of every boundary; thin IO shells around them.
