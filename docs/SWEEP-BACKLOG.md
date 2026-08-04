# Sweep backlog — 2026-08-01

Synthesis of a six-lens exhaustive sweep (docs, refactoring, performance, test-coverage, feature-roadmap,
correctness) of the cb-testing harness after the P1/P2/P3 + 2-helix work landed. Items are grouped by kind;
each carries a rough **value**, **effort** (S/M/L), and file:line. The **Do-first** list at the top is the
cross-cutting priority order.

## Do-first (highest leverage, in order)
1. **[BUG/CRITICAL] Relay death mid-window → false GREEN.** `relay_pipeline.rs:296-333` + `report.rs:136`.
   If relays are alive at preflight but OOM-die during the observation window (the exact scenario this repo
   exists to catch), the tier-1 relay checks emit SKIP, and `exit_code` treats a tier-1 SKIP as pass →
   exit 0 / overall PASS with the MEV pipeline unverified. **Fix:** a tier-1 check that was expected to run
   (relay alive at preflight) but degraded to SKIP must FAIL, or `exit_code` must treat unexpected tier-1
   SKIP as failing. Effort S/M.
2. **[BUG/HIGH] Warmup 5xx → false RED (the harness is red on green runs today).** `cb_metrics.rs:227` +
   escalation at `:636`. The matrix reads absolute *cumulative* status-code counters from one end-of-run
   scrape and FAILs on any relay-side 5xx > 0, incl. pre-window warmup; escalated to tier-1 → whole run
   FAILs. This is the `df69c90` "residual FAIL is transient warmup 5xx" — a real false red. **Fix:** delta
   the counters over the observation window (the `--live-metrics` path already grabs a baseline at
   `main.rs:656`), or a small tolerance, or keep matrix-5xx tier-2/WARN unless `--strict`. Carefully (don't
   mask a *sustained* 5xx). Put the threshold in the pure classifier with tests on both sides. Effort S/M.
3. **[DOCS] Fix the flashbots→2-helix staleness cluster.** README:28/61/62 + `.env.example:13-15` still say
   multi-relay = "helix + flashbots"; it's now two helix instances (flashbots = builder only). Also add
   `relay.best_bid` to the README check list and `--skip-finalization-check` to the CLI docs. Effort S.
   (Being done this session.)
4. **[MIGRATION DEBT] Finish the 2-helix migration honestly** — the flashbots-era assumptions the migration
   left stale: `discovery.rs:428-470` post-mortem salvage queries the dead flashbots Postgres
   (`mev-relay-postgres`/`mev_boost_relay`) → C1's safety net is inoperative + misattributes the error;
   `relay.rs:62-112` pagination assumes flashbots descending-order + cursor semantics, unverified for helix.
   Effort M.
5. **[DOCS] New cross-cutting docs** (in flight): `docs/CHECKS.md` (check catalog + the WARN-non-fatal exit
   contract), `docs/ARCH.md` (how it all fits), `docs/fork-delta.md` (the ethereum-package divergence);
   re-stamp P1/P2/P3 plan statuses (all shipped but read "uncommitted/backed-out") + add `docs/plans/INDEX.md`.

## Bugs / correctness (from the correctness + feature sweeps)
- **C1 (Critical)** relay-death false-green — see Do-first #1.
- **H2 (High)** warmup-5xx false-red — see Do-first #2.
- **H3 (High)** `check_mev_delivery_rate` counts only the FIRST relay's deliveries (`relay_pipeline.rs:116-140`
  `break`s on first Ok) → undercounts in mux/2-relay → spurious WARN. Fix: union across relays like
  `check_payloads_delivered_multi` does. Effort S.
- **M4 (Med)** finality gate/threshold mismatch: `chain_health.rs:9-23` demands `finalized_epoch>=2` but the
  skip gate is `end_slot<96` (`:179`); a window ending in ~[96,160) runs the check on a chain that has only
  finalized epoch 1 → false tier-1 FAIL. Fix: raise the gate to ~slot 160 or lower the threshold. Effort S.
- **M5 (Med)** post-mortem salvage targets dead flashbots Postgres — see Do-first #4.
- **M6 (Med)** helix pagination contract unverified — see Do-first #4.
- **M7 (Med)** general form of C1: tier-1 SKIP universally treated as pass (`report.rs:136`).
- **CI bug** `integration.yml` sets `ENCLAVE: cb-ci-<run_id>` but never passes `--enclave "$ENCLAVE"` to
  run-and-verify.sh (defaults to CB-Testnet) → teardown targets the wrong enclave. Also pins
  `commit-boost/pbs:latest` (non-reproducible; the file's own NOTE questions it) and runs only `cb-basic`
  (no mux/multi-relay e2e). Effort S (bug) / M (matrix).
- **L (low, conscious tiering)** payload conflict/mismatch only WARNs (`payload_matching.rs:145`); missed
  counts beacon errors (`chain_health.rs:50`); best_bid degenerate with identical 2-helix bids (needs
  distinct keys); mux check verifies pubkey→mux, not mux→relay-URL.

## Documentation (from the docs sweep — top-10 plan)
1. Fix flashbots staleness (Do-first #3). 2. Re-stamp plans + `docs/plans/INDEX.md`. 3. `docs/CHECKS.md`
(catalog + exit contract). 4. `docs/fork-delta.md` + configure an `upstream` remote. 5. `docs/ARCH.md`.
6. Refresh README module map + `sim` verb contract (generate/preflight/triage + `--check`). 7. "How to add a
check / a scenario" contributor guide (lift P3's classify-seam recipe + the Scenario/Images/golden pattern).
8. Anchor load-bearing WHY-comments (the exit contract at `report.rs`/`main.rs:559`; the discovery↔orchestrator
kurtosis-shell duplication; `cb_metrics.rs` as the seam model; the now-inert `mev_relay_image` branch at
`scenario.rs:243-259`). 9. De-mislead the fork's bundled `kurtosis-ethereum` skill (points at upstream).
10. Reconcile the unverified #1384 "exit" claim (README states as fact what NORTH-STAR flags unverified);
move the P0-P5 status ledger out of NORTH-STAR into the plans INDEX.

## Tests (from the test-coverage sweep — top-10)
Unit-testable NOW: 1. `report::exit_code` contract (untested — the CI verdict itself). 2. `discovery::parse_services`
(fixture exists). 3. `orchestrator::{enclave_name,resolve_configs}`. 4. `best_bid::value_eth_to_wei` edges.
5. payload combined case (2 relays agree; mismatch+conflict). Needs a classify seam first (the P3 pattern):
6. `classify_mev_delivery(mev_blocks,total,threshold)`. 7. per-relay aggregation fold →
`fn worst(statuses)->CheckStatus`. 8. `classify_missed_slots`. 9. `classify_cb_running`. 10.
`parse_cbverify_report` (orchestrator's cb-verify-JSON seam — a format-drift trap → `result="unknown"`).
Process: add a `kurtosis run --dry-run` CI job over `configs/generated/*.yml` (the Starlark fork has ZERO
tests; dry-run catches launcher/config defects in seconds); extend the nightly beyond `cb-basic`; pin the CB
image to a digest. **Note the golden circularity:** the 3 multi-relay goldens are self-generated (no baseline
since the Python is gone), so their byte-match proves determinism, not correctness — only the devnet validates
them; add more content-anchored assertions like `golden_images_are_the_baked_defaults`.

## Performance (from the perf sweep — the check pipeline is entirely serial)
Biggest bang, low risk, in order: 1. Four checks re-fetch the same paginated `get_payloads_delivered`
(`relay_pipeline.rs:60,117`, `payload_matching.rs:27`, `best_bid.rs:111`) — fetch once per relay in
run_verification, share (~4× the heaviest relay traffic). 2. Discovery prefers a `kurtosis port print`
subprocess per port though `enclave inspect` already parsed them (`discovery.rs:316,333,359,371`) — invert
precedence (multi-second startup win). 3. CB logs fetched twice (best_bid + mux_routing) — fetch once, share;
and `join_all`/spawn_blocking the per-service fetches. 4. Serial per-slot beacon loops → `buffer_unordered(16)`
(`relay_pipeline.rs:149` mev-rate ~2-3s; `chain_health.rs:49` missed-slots ~1.5s; payload_matching per-slot).
5. `validator_registrations` O(validators×relays) sequential (`relay_pipeline.rs:223`). 6. Fat LTO on an
I/O-bound binary that rebuilds every run (`Cargo.toml:53`) → `lto="thin"` cuts the per-edit relink 30-90s →
~5-15s. 7. `check_cb_running` duplicate `enclave inspect` (`chain_health.rs:105`). 8. `--image-download always`
re-pulls every run (gate to CI). 9. Default `--target-epoch 7` = ~57min chain-time wait — the dominant
wall-clock; measure whether builders warm up earlier and lower it (coverage tradeoff). `futures-util` is
already in the lock tree.

## Refactoring (from the refactoring sweep)
1. **Delete `test_mux`/`test_relay` bins** (823 lines, duplicate library code; `test_mux` is a *regressed*
pre-P3 copy that can print false greens; on the NORTH-STAR KILL list). HIGH value, LOW risk. 2. Split
`mux_routing.rs` — extract the shared CB-log primitive (`strip_ansi`/`CbEvent`/`parse_cb_log_line`/
`fetch_service_logs`) to `checks/cb_log.rs` (best_bid imports it from mux_routing — mis-homed). 3. Drop the
blanket `#![allow(dead_code)]` (`lib.rs:8`, `main.rs:6`) + delete confirmed-dead code (metrics.rs:56-98
helpers, beacon.rs:101-139 helpers, relay.rs:142 `is_validator_registered`, orchestrator.rs:91 `EnclaveState`
write-only, discovery `relay_identities` dead field, `CauseKind::Unknown`). 4. Collapse the three hand-rolled
worst-status folds in `run_relay_checks` → `impl Ord for CheckStatus` / `fn worst(iter)` + extract
`classify_mev_rate`/`classify_registrations` (the real Law-4 seam gap). 5. One `http_client(timeout)` ctor
(7 sites). 6. Shared `run_kurtosis` helper (13 sites). 7. orchestrator should call the lib (BeaconClient for
polling; long-term lift `run_verification` into a lib entry both bins call) instead of re-shelling +
untyped-JSON-parsing. 8. Dedup Prometheus counter extraction (10 sites) + live.rs's 3 near-identical delta
arms. 9. Tame `main.rs::run_verification` (400 lines; 7× copy-pasted error-report block → a closure). 10. `sim`
shell-runner dedup + single `strip_ansi` (3 copies).

## Features / roadmap (from the feature sweep — ranked)
Highest leverage next moves: 1. **Feature-fired assertions (Law 3)** — skip-sigverify/extra-validation/
timing-games are non-tests (run only generic checks); add a positive per-scenario assertion arm. M. 2. **Warmup
tolerance** (= bug H2). S/M. 3. **Image/build provenance in the report** — records no digests, blocks
regression detection + the P4 bump/diff. S/M. 4. **`sim run` verb** — fold run-and-verify.sh + cb-orchestrator
into Rust, own the pull → digest-pinned preflight (closes the `--image-download always` false-green window) +
mid-run triage (P5 spine). L. 5. **ePBS first slice** — the fork is already ePBS/external-builder-ready
(Law 6b done); add `Scenario::Epbs` (mev_type:epbs, gloas_fork_epoch:0) + an epbs-branch CB image + a
`get_execution_payload_bid` cb_metrics arm + an envelope-flow beacon check. M/L. Next tier: 6. distinct-key
2-helix for real divergent competition. 7. Law 7 EL/CL matrix (fork already supports it; cb-testing-side
change). 8. auto-triage on mid-run death. 9. `sim diff` (verdict diff between runs). 10. `sim doctor` (host
prereqs). 11. `sim checks --list --json` (machine-readable catalog). P4 cluster: 12. upstream remote + tag pin
+ `bump` workflow. 13. shrink the fork overlay (drop stubbed zkboost). 14. CB-image preflight — recommend
keeping the honest Inconclusive stub (partial would false-pass `[pbs]` drift). 15. kill test_mux/test_relay
(= refactor #1). 16. fix + expand the nightly CI (matrix + build-cb-main lane + the enclave-name bug). 17.
snapshot relay-data-API during the window (relay-dies-before-checks class). 18. structured run history / trend.

### Feature status (updated 2026-08-01, campaign)
- DONE: #3 image/build provenance (`report::Provenance`, `c9d18e4`); #10 `sim doctor` + #11
  `sim checks --list --json` (`e6185c6`); #9 `sim diff` (verdict/provenance regression gate); #1
  Law-3 feature-fired assertions (`feature.timing_games` / `.extra_validation` / `.skip_sigverify`).
  H2/H3/C1/M4-M6 all landed earlier in the campaign.
- **#6 divergent-bid 2-helix DONE + LIVE-VALIDATED (2026-08-03):** rbuilder upstream supports per-relay
  bid values from ONE instance (top-level `[[subsidy_overrides]]` name-matched to `[[relays]]`; the
  ethpandaops image is upstream develop, no fork) — the previously-mapped 5-file two-builder surgery
  was unnecessary. `mev_builder_subsidy` accepts a list (submodule fc5e6a2); cb-multiple-relays emits
  `[1, 2]`; `relay.best_bid` gained `divergent_slots` + a discrimination-vs-degenerate-tie detail.
  Live run: overall PASS, **65/65 competitive slots divergent, 0 suboptimal** — CB delivered the
  higher (+1 ETH) bid on every slot (e.g. slot 5: relay_0 1.0439 vs relay_1 2.0439 ETH). NOTE:
  submodule fc5e6a2 is local-only (with 43fe436+fbe3141) — J must push to JasonVranek/ethereum-package.
- NEXT: #7 Law 7 EL/CL matrix.
- **NEW (surfaced by Law-3):** bad-signature-injecting helix mock relay — the only way to turn
  `feature.skip_sigverify` from an honest WARN into a real ON/OFF differential test (ON delivers the
  bad-sig bid, OFF rejects with `ValidationError::Sigverify`). Needs a fault mode in
  `src/bin/sim/genmodel/helix.rs` + the relay mock. M/L. Same capability would sharpen any negative-path
  feature test. Filed as a follow-up, not built.
- **DEFERRED — do NOT autonomously build:** #5 ePBS first slice belongs to the `/epbs` arc, where J
  gates every design decision and nothing commits without his review. Surface it to J; don't self-drive it.

### skip_sigverify differential — CLOSED 2026-08-04 (live, both arms)
The bad-signature follow-up filed below is DONE, and it needed no relay/mock change: CB validates
bids against the pubkey in its OWN `[[relays]]` url, so pointing CB at the real helix through a
literal url carrying a valid-but-WRONG BLS key (a mnemonic validator key, not helix's
DEFAULT_MEV_PUBKEY) poisons exactly the function `skip_sigverify` skips. Scenarios
`cb-sigverify-diff` (treatment) + `cb-sigverify-diff-control`. Live: treatment = overall PASS, 65
payloads, **222 auction winners**, `feature.skip_sigverify` PASS; control (same poison, skip OFF) =
overall FAIL, **ZERO payloads delivered**. The falsifier held — the flag is the only variable and the
pipeline flips from dead to healthy.
- **Bonus defect found by dogfooding:** running `sim diff` across the two arms reported REGRESSION on
  a run that went FAIL -> PASS. `Direction` was derived from `CheckStatus`'s worst-status `Ord`, so
  `SKIP -> PASS` (a check that started running and passed) read as a severity increase. Fixed: Skip
  transitions are now `CoverageGained`/`CoverageLost`, never regressions; same rule backs
  `overall_regressed`. Verified against the two real reports (2 false REGRESSEDs -> cov-gain, the one
  genuine `cb_relay_latency PASS -> WARN` retained).

### The prysm question, sharpened by the sweep (2026-08-04)
With the v2 route enabled, the SAME relay route behaves oppositely per CL:
- **lighthouse**: CB submits over the relay's v2 route -> **202 Accepted**, 222 deliveries, run PASSes.
- **nethermind+prysm**: CB submits over the same v2 route -> **4xx on every one of 25**, zero payloads.

So the variable is NOT the route (now enabled and demonstrably working) and NOT relay capability - it
is what CB forwards when the request ORIGINATES from prysm. Prysm calls CB's own
`/eth/v2/builder/blinded_blocks` (confirmed in CB logs, `ms_into_slot=256`, so not a timing issue),
whereas lighthouse calls CB's v1 endpoint and CB still reaches the relay over v2. Next hypothesis to
test: the prysm-originated body/encoding (prysm negotiates SSZ; CB gained SSZ submit_block support in
#468) produces something helix refuses, or the v2-originated path forwards different content than the
v1-originated one. Method: `--keep` run, read helix's own rejection reason for a v2 submission, and
diff the CB request headers/content-type between the lighthouse and prysm paths.

### NORTH STAR REACHED: the CB signer module runs on Kurtosis (2026-08-04)
`signer.pubkeys` **PASS**: *"signer loaded all 128 validator key(s) and authenticated the module JWT"*,
on a run whose overall verdict is also PASS. First time the Commit-Boost signer has ever been
exercised on a Kurtosis devnet - the ethereum-package had zero signer support.

What the single assertion actually proves, by construction:
- the container launched and bound its port (the port `wait` would have failed the run otherwise),
- it read the devnet's EXISTING validator keystores - no new key material - and decrypted **all 128**,
  which is exactly `num_validator_keys_per_node`; the teku-keys/teku-secrets choice was the difference
  between this and a healthy signer holding ZERO keys,
- a hand-rolled HS256 module JWT authenticated, so the route binding and the null `payload_hash` claim
  were both right,
- and PBS was undisturbed by `[signer]`/`[[modules]]` in the shared config.

Cost after the adversarial grill: ONE devnet run, spent on a one-line type error
(`el_cl_genesis_data` is a UUID string in main.star, not the struct the web3signer launcher receives).
Every trap the grill named - the 0600 `secrets/` dir, the 127.0.0.1 host default, the silent exit-0,
the 429 self-poison, the `commit-boost-*` name collision - was avoided before it could fire.

**Caveat:** the signer's own container logs were NOT captured (my capture script's grep matched the
enclave name instead of the service, and the script tore the enclave down despite `--keep`). So
`loaded_consensus=N` was never read as independent corroboration. The evidence is the check itself,
which FAILs on zero keys by construction and reported the exact expected count.

### OVERNIGHT SWEEP RESULTS (2026-08-04) — 8 established + 4 follow-ups
**Sweep 1 (8 established scenarios, fixed helix config): 7 PASS, 1 expected-FAIL.**
cb-basic, cb-mux, cb-skip-sigverify, cb-timing-games, cb-extra-validation, cb-sigverify-diff all PASS.
cb-sigverify-diff-control FAILs **by design** (it is the control arm: same poisoned relay pubkey with
skip_sigverify OFF, so zero payloads is the proof the differential is real). cb-multiple-relays FAILED
on a false red (see below), was fixed, and **PASSES on re-run**.

**Sweep 2 (4 follow-ups): 1 PASS, 1 expected-FAIL, 1 harness bug, 1 running.**
- cb-multiple-relays re-run: **PASS** - confirms the beacon-side fix.
- **cb-min-bid: expected-FAIL, and the feature check PASSED**:
  `min_bid_eth enforced ✓ 221 bid(s) rejected below the 0.5 ETH floor, and no winner was under it`.
  Zero payloads delivered is the DESIGNED outcome (the floor rejects every bid), so like
  cb-sigverify-diff-control this is a fault-injection scenario whose overall verdict is FAIL by
  construction. **Both belong on a known-expected-FAIL list** so a sweep summary is not misread.
- **cb-signer: did not launch.** Starlark evaluation error, `string has no .files_artifact_uuid field
  or method` at signer_launcher.star:104. main.star passes `el_cl_data_files_artifact_uuid` (already a
  UUID string) while the web3signer launcher I copied receives the el_cl_genesis_data STRUCT. One-line
  fix, comment added at the site. The signer is still UNPROVEN - it has never started.
- cb-basic-nethermind-prysm: running (expected FAIL, known interop gap).

**Method note:** a `.star` file that parses is not a `.star` file that runs - python-ast parsing caught
syntax but not this type error. Only a devnet run exercises the launcher, so the signer needed its own
run to find a one-line bug.

### Sweep finding: submit_blinded_block was reading the wrong side of the proxy (2026-08-04)
`cb-multiple-relays` FAILED at 186/626 relay-side 5xx (29.7%) on a run that was otherwise flawless -
65/65 payloads across 2 relays, 100% MEV rate, 65/65 hashes matched, best_bid verified over 65
competitive slots, 0 missed slots. Per-relay data settled it in one look:
```
mev_relay_0 (subsidy 1, LOSES every auction): 202x1,   4xx x219, 5xx x185
mev_relay_1 (subsidy 2, WINS every auction):  202x219, 4xx x1,   5xx x1
beacon side:                                  202x220   <- CB served the CL every time
```
CB asks EVERY relay for the payload; only the winner has it, so losers error by construction. **My own
divergent-bid feature caused the false red**: making relay_1 win every slot means relay_0 now fails
every slot, taking the rate from 18% (wins split) to 29.7% and across the 25% line. Fixed by judging
the endpoint on the BEACON side (did the proposer get its payload?), keeping relay-side codes as
context. NOT a loosening - the nethermind+prysm run's beacon side is 26x 5xx and still FAILs, and both
fixtures are pinned as tests. **Lesson, same shape as the 555 and 556 findings: an expected, benign
condition counted as a relay error.** Three for three now; when a rate-based check fires, ask first
whether the denominator contains events that are correct by design.

### Overnight sweep plan + results-so-far (2026-08-04)
Two chained sweeps so the box runs continuously; sweep 2 WAITS for sweep 1 rather than competing for
the devnet (two live enclaves risks the cgroup-OOM class already paid for).
- **Sweep 1** (the 8 established scenarios, on the fixed helix config): cb-basic **PASS**,
  cb-mux **PASS** (mux.routing verified all 224 routing decisions; the two WARNs are correct by
  design - best_bid cannot compete when each validator is pinned to one relay, and 6.7% 5xx is under
  the H2 threshold). Remaining: skip-sigverify, multiple-relays, timing-games, extra-validation,
  sigverify-diff + its control.
- **Sweep 2** (the three scenarios added after sweep 1 launched): cb-signer FIRST (the North Star,
  wholly unproven), then cb-min-bid, then cb-basic-nethermind-prysm (known-fail, for the record).
  The signer run banks the container's own logs before teardown - `loaded_consensus=N` and any
  key-loading warnings are the diagnosis if the count assertion fails, and they die with the enclave.
- **Caveat repeated:** run-and-verify.sh rebuilds cb-verify per scenario, so scenarios launched
  before a code change use the older binary. Compare check-by-check only within a settled tree.

### UN-RETRACTED with real evidence: GetPayloadV2 DID change behavior (2026-08-04, sweep)
The retraction below was correct that the false PASS proved nothing. The sweep then supplied proper
evidence from the DELIVERY counters, which no broken check was involved in:
- **pre-fix** (route disabled), three separate runs: `submit_blinded_block: 222 v1 (200), 0 v2 (202)`
- **post-fix** (route enabled), cb-basic: `submit_blinded_block: 0 v1 (200), 222 v2 (202)`

A complete flip, on **lighthouse** - so enabling helix's `GetPayloadV2` changed the relay-side path for
every scenario, not just the prysm one. CB submits to the relay's v2 route and gets 202 Accepted where
it previously used v1 throughout. The route addition is therefore confirmed effective; what remains
unexplained is only why nethermind+prysm still has its blocks REJECTED on that now-working route.

**Sweep caveat - scenarios do not all run the same binary.** `run-and-verify.sh` rebuilds `cb-verify`
at the start of EACH scenario, so a sweep spanning code changes mixes versions. cb-basic ran before
the v2-metric fixes landed, so its `cb_relay_v2_unsupported PASS` and `cb_v2_fallback PASS` come from
the BROKEN checks; later scenarios use the fixed ones (v2_fallback now SKIPs). Compare check-by-check
across a sweep only when the binary is pinned, or re-run the early scenarios after the code settles.

### RETRACTION + two dead checks found by a config-surface audit (2026-08-04)
An independent audit of CB's config/metric surface caught two of our checks reading metric names that
CANNOT exist, and one of them invalidates a claim made earlier the same day:
- **`cb_relay_v2_unsupported` never fired.** CB's PBS registry is
  `Registry::new_custom(Some("cb_pbs"))`, and this counter is *registered* as
  `pbs_submit_block_v2_unsupported_total`, so the EXPOSED name is
  `cb_pbs_pbs_submit_block_v2_unsupported_total` - a doubled `pbs_`, unlike every sibling metric
  (`relay_status_code_total` -> `cb_pbs_relay_status_code_total`). We matched the registered name, so
  the check reported PASS on every run by construction.
- **RETRACTED:** "the GetPayloadV2 fix worked" was asserted on that check's PASS in the alt-pair run.
  That PASS was structural, not evidence. The submit_blinded_block FAIL looks the SAME before and
  after the route fix (26 rejected -> 25 rejected), so **whether enabling GetPayloadV2 changed
  anything is now UNVERIFIED**. Re-test with the fixed check before claiming it again. The route
  addition is still correct on its own merits (helix does expose `GetPayloadV2`), but its effect is
  unproven.
- **`cb_v2_fallback` was permanently green.** It read
  `cb_pbs_submit_block_v2_fallback_to_v1_total`; commit-boost registers no `*fallback*` metric at all.
  Absence was treated as "zero fallbacks == PASS", so it could never fail. Now returns SKIP naming
  itself inert and pointing at the check that actually owns v2 support. A check that cannot fail is
  worse than no check.
- **Method rule bought here:** verify metric names against a REAL scrape, never against the
  registration constant in CB's source - the registry prefix is applied at gather time.

### Law 7 alt-pair: layer 2 — helix accepts the v2 route but REJECTS the block (2026-08-04)
The `GetPayloadV2` fix WORKED and is confirmed by the new check: `cb_relay_v2_unsupported` = PASS
("No v2-unsupported submissions"), i.e. the 404-on-v2 is gone. But the pair still delivers zero
payloads: `cb_submit_blinded_block_matrix` = FAIL, "the relay REJECTED all 25 blinded block(s)" - a
genuine 4xx from a route that now exists. So the failure moved one layer down, from
route-not-enabled to block-refused, and the cause is again unknown.
- Both new diagnostics behaved exactly as designed: `cb_relay_v2_unsupported` distinguished
  "route missing" from "block rejected", and the rewritten submit_blinded_block detail correctly said
  REJECTED rather than the old "proposer never chose a builder block".
- Missed slots 14.1% (was 17.2%) - still far above the 0.00% that geth+lighthouse hits on the same box.
- **NOT chased further yet**: this is a NEW scenario's open issue, not a regression in the established
  ones, and the priority is the full MEV sweep. Next step when resumed: a `--keep` run reading helix's
  own rejection reason for a slot on the v2 route (the same method that found the GetPayloadV2 gap).
- **Treat `cb-basic-nethermind-prysm` as a KNOWN-FAIL scenario** until closed, like the sigverify
  control arm - it documents a real interop gap rather than a broken harness.

### Law 7 FIRST DIVIDEND — nethermind+prysm cannot complete an MEV block (2026-08-04, live)
The alt-pair scenario ran on its first devnet and **found a real cross-client failure that
geth+lighthouse hides** — exactly what Law 7 predicted. Overall FAIL. The pipeline works right up to
the last hop:
- prysm asked CB for headers 40x, got **26 bids** (`cb_get_header_matrix` PASS); 128 validators
  registered; relay received **1318 builder blocks**; chain finalized (epoch 5).
- prysm then submitted **26 blinded blocks**, CB forwarded all 26 to helix, and **helix rejected
  every one with 4xx** (`submit_blinded_block` relay_side `{"4xx": 26}`, beacon_side `{"5xx": 26}` =
  CB returning 502 to prysm). **Zero payloads delivered**, 0% MEV rate. Also 17% missed slots.
- **Cause NOT yet determined** — it is relay-side rejection of prysm-proposed blinded blocks (block
  seen as invalid/late/misencoded by helix), not a proposer-side or wiring gap: the fork DOES wire
  prysm (`--http-mev-relay` on the CL, `--enable-builder` on the VC) and prysm did everything asked
  of it. Next step: re-run with `--keep` and read helix's rejection reason for a slot.
- **Second dividend (a defect in our own harness, fixed):** `cb_submit_blinded_block_matrix`
  reported "proposer never chose a builder block" — the exact opposite of what happened — because it
  diagnosed purely from `200+202 == 0`. A wrong-component diagnosis is worse than none. Now split:
  submissions-present-but-all-rejected FAILs naming the relay; genuinely-zero-submissions keeps the
  original WARN.

### Law 7 (EL/CL matrix) — config-gen slice landed 2026-08-04
`ElCl` axis threaded through participants AND every derived service name; new
`cb-basic-nethermind-prysm` scenario. Caught in passing: extra-validation's `rpc_url` was hardcoded
to `el-1-geth-lighthouse` while the ethereum-package names EL services `el-{index}-{el}-{cl}`, so on
any other pair it pointed at a nonexistent service and the feature silently no-oped. Now derived.
All 8 pre-existing goldens byte-identical. **Still owed: a live devnet run on the alt pair**
(needs nethermind/prysm image pulls).

### Live-validation findings (2026-08-02, cb-timing-games run)
- **VALIDATED live:** provenance populates (config_hash + all 4 image sha256 ids); `feature.timing_games`
  = PASS ("fired ✓ 1342 CB debug log lines" — the Law-3 mechanism works on real infra); `relay.best_bid`
  = PASS (37 competitive 2-helix slots); H2 rate-classifier discriminates correctly (see below).
- **RESOLVED 2026-08-03 (root-caused, no design exemption needed):** the "47.5% relay 5xx" was
  **entirely CB's synthetic code 555** (`TIMEOUT_ERROR_CODE` — client-side deadline cancellation),
  which `bucket_code` lumped into 5xx. Evidence: dedicated capture run's raw counter showed get_header
  = 200×48 / 204×6 / 400×4 / **555×42, zero real 5xx**; helix logs had 0 server errors; CB log had 45
  timeout mentions. Fix: 555 → its own `timeout` bucket; >25% timeouts = WARN (never FAIL, not under
  --strict — it's CB's own deadline policy, e.g. timing-games by design); real 5xx keeps the rate-FAIL
  with timeouts excluded from the denominator so an error storm still fails amid heavy polling.
  4 new both-sides tests. **Live-confirmed post-fix (2026-08-03):** the timing-games re-run now
  reads overall **PASS**, exit 0, with the annotative WARN "396/892 CB-deadline timeouts (555,
  44.4%) ... not a relay-served error (486 bids still delivered)"; `feature.timing_games` PASS again
  (1342 TG: lines). The original finding below is kept for the record:
- **ORIGINAL FINDING (now resolved) — timing-games tier-1-FAILs on `cb_get_header_matrix` despite a green
  pipeline.** The run showed get_header 47.5% 5xx (424/892, evenly split mev_relay_0=214 / mev_relay_1=210)
  → matrix FAIL → tier-1 → overall FAIL. This is NOT the H2 warmup false-red (the rate genuinely exceeds
  25%, so the fix is working); it is the aggressive timing-games config (target_first_request_ms=100,
  timeout_get_header_ms=400, frequency=200) polling helix before it has a bid, and helix answering 5xx.
  The DELIVERY pipeline was fully green (best_bid / payloads_delivered / payload_hash / mev_delivery all
  PASS), so MEV worked — the 5xx are the expected consequence of the feature under test. **Question for J:**
  should timing-games exempt / down-tier the get_header matrix (it fails on its own aggressive-polling
  behavior), or is a high early-poll 5xx rate a real signal worth failing on? Could not root-cause the
  exact 5xx source (enclave torn down); confirm with `--keep` + relay logs next run. NOT patched.
- **CONFIRMED scenario-specific:** the SECOND run (cb-extra-validation, a normal non-aggressive
  scenario) had `cb_get_header_matrix` = PASS with **0 get_header 5xx** ("222 bids delivered, 0 no-bid,
  1 4xx") → overall PASS. So the H2 rate-classifier does NOT false-fail a green run (the no-false-red
  guarantee, validated live), and the timing-games FAIL is definitively the aggressive config's own
  behavior, not a harness bug. Also validated live: `feature.extra_validation` = PASS (second Law-3
  marker check fires), `relay.best_bid` = SKIP on the single-relay scenario (correct), and `sim diff`
  on the two REAL reports (FAIL→PASS, feature checks added/removed per scenario, config_hash delta,
  correct "no regression" verdict).

## Notes
- P3 is effectively COMPLETE despite the plan doc reading "best_bid backed out" — best_bid v2 re-landed
  (`145d7e1`). Law 6b (external-builder hook) is already satisfied in the fork.
- The two defects that mis-verdict a NORMAL run today are C1 (false green on relay death) and H2 (false red
  on warmup 5xx). Everything else is hardening, ergonomics, or reach.
