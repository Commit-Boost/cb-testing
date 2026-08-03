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
- NEXT (in order): #6 distinct-key 2-helix (divergent competition) → #7 Law 7 EL/CL matrix.
- **NEW (surfaced by Law-3):** bad-signature-injecting helix mock relay — the only way to turn
  `feature.skip_sigverify` from an honest WARN into a real ON/OFF differential test (ON delivers the
  bad-sig bid, OFF rejects with `ValidationError::Sigverify`). Needs a fault mode in
  `src/bin/sim/genmodel/helix.rs` + the relay mock. M/L. Same capability would sharpen any negative-path
  feature test. Filed as a follow-up, not built.
- **DEFERRED — do NOT autonomously build:** #5 ePBS first slice belongs to the `/epbs` arc, where J
  gates every design decision and nothing commits without his review. Surface it to J; don't self-drive it.

### Live-validation findings (2026-08-02, cb-timing-games run)
- **VALIDATED live:** provenance populates (config_hash + all 4 image sha256 ids); `feature.timing_games`
  = PASS ("fired ✓ 1342 CB debug log lines" — the Law-3 mechanism works on real infra); `relay.best_bid`
  = PASS (37 competitive 2-helix slots); H2 rate-classifier discriminates correctly (see below).
- **NEW FINDING (J design call) — timing-games tier-1-FAILs on `cb_get_header_matrix` despite a green
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
