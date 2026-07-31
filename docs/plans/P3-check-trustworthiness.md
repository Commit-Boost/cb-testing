# P3 — check trustworthiness: kill the false-greens, make verdict math testable

> **PROPOSAL — awaiting J's go.** Everything here CHANGES VERDICT LOGIC (what the harness reports as
> pass/fail), so it is deliberately NOT started autonomously. It is written so J can approve a direction
> in one line. Evidence is code-confirmed (file:line below).

**Goal:** A harness that lies green is worse than an ugly one (North Star Law 3). Three checks currently
report PASS while having verified nothing — fix them, and remove the structural reason they hid.

## The structural root cause (why these bugs exist and stayed hidden)
All three checks **inline their decision boundary inside an async network-fetch function.** There is no
pure `classify_*(data) -> CheckResult` seam (unlike `cb_metrics.rs`, which cleanly separates
`classify_endpoint()` from I/O and consequently has 19 decision tests and no known false-greens). No seam →
no unit test without a devnet → the decision logic was never exercised → bugs survive. So the Law 3 fix
(no false greens) and the Law 4 fix (unit-test the verdict math) are the SAME move: **extract the pure
classifier, then test + correct it.**

## The three confirmed false-greens (code-verified)
1. **`mux_routing.rs:678` — passes having verified zero routing decisions.** The pass branch fires whenever
   `violations.is_empty()`, and the message counts `total_events` (raw log lines). If CB debug logging is
   off, there are log events but zero parseable routing decisions (`pubkeys_verified == 0`), and it reports
   "All N mux routing decisions verified ✓" — N being log lines, not verified decisions. Only
   `total_events == 0` is guarded (→ WARN); `events>0, pubkeys_verified==0` falls through to PASS.
2. **`relay_pipeline.rs:51` — best-bid is a first-wins union by slot.** `by_slot.entry(slot).or_insert(...)`
   counts DISTINCT delivered slots; it never compares bid VALUES across relays. One delivering relay scores
   identically to genuine two-relay aggregation — the "aggregated bidding" the multi-relay scenario exists
   to test is never actually checked.
3. **`payload_matching.rs:22` — first-wins union by slot drops relay disagreement.**
   `by_slot.entry(p.slot).or_insert(p.block_hash)` keeps only the FIRST relay's hash per slot, so when two
   relays report different `block_hash` for the same slot (relay equivocation — the exact misbehavior this
   cross-check exists to catch), all but the first are discarded before the on-chain compare. Verdict is
   **order-dependent**: honest relay first → PASS (false green); bad relay first → the honest hash is
   dropped. Happy path is unaffected (a slot's winning relay is unique and agrees), so it bites precisely in
   the adversarial multi-relay case. (Also noted while here: `missed` — relay delivered a slot with no
   on-chain block — is counted but never downgrades the verdict; and a mismatch is WARN, not FAIL.)

## Status (2026-07-31): 2 of 3 LANDED + reviewed; best-bid backed out pending correct source
- **mux.routing ✓ LANDED** (`2868e8e`): pure `classify_mux_routing`; WARNs on `routing_decisions_verified==0`.
- **payload_hash_match ✓ LANDED** (`cad323a`, `37ebd75`): pure `classify_payload_matches`; detects per-(relay,slot)
  conflict; WARN.
- **relay.best_bid ✗ BACKED OUT** (`effc560` reverted by `4b873af`): the adversarial review found the data
  source unsound — `get_builder_blocks_received` includes builder bids that failed sim and were never
  offered to the proposer, so it would false-alarm on correct runs; sampling also made "no competition"
  WARN the default. The pure `classify_best_bid` logic was fine. **Correct follow-up:** source per-relay
  bids from CB "received new header" log events (`relay_id`+`slot`+`value_eth`, already parsed by
  `parse_cb_log_line`, full-coverage), parse `value_eth` decimal→wei for `Ord`, compare delivered vs max
  OFFERED bid. Needs CB-log context (enclave + cb_service_names) plumbed into the check — its own reviewed
  slice. `check_payloads_delivered_multi` stays as the coverage check meanwhile.

**Review confirmations / notes for J:**
- **WARN is non-fatal** (`report.rs:136`, `main.rs:547`): exit code / overall result key ONLY on a tier-1
  FAIL. So these PASS→WARN changes do NOT break CI/the nightly. BUT that means a real anomaly these
  trust-core checks detect (relay equivocation; unverifiable routing) yields a GREEN exit — a CI consumer
  must parse the JSON `result:"WARN"`, not just the exit code. Decide if any of these should be able to FAIL.
- Lenient gaps left as-is (within your ratified scope; flagged for possible future tightening): payload
  `missed` (relay delivered a slot with no on-chain block) never downgrades the verdict and is
  indistinguishable from a transient beacon error; a mux "using mux config" event for a pubkey our TOML
  parser missed folds into the generic WARN rather than its own parse-gap signal.

## J's decisions (2026-07-31) — RATIFIED
- **mux.routing:** unverifiable (`pubkeys_verified==0`) → **WARN** (not PASS), and CB debug logging is **required**
  for mux scenarios. Finding: the generated cb-mux config ALREADY sets `[logs.stdout] level="debug"`, so the
  requirement is mostly satisfied by generation; fix = the WARN gate + a guard/assert that debug stays on.
- **payload_matching:** a cross-relay `block_hash` conflict → **WARN** (matches how single-relay mismatches
  are already handled). The BUG is that the first-wins union DROPS the conflict before it's ever detected, so
  the fix is the DETECTION (compare per-(relay,slot)); verdict stays WARN, naming the slot + relays.
- **relay_pipeline best-bid:** **compare bid values**. Per-relay get_header bids are in the CB logs
  ("received new header … value_eth … relay_id"). Fix = ADD a best-bid check arm that asserts CB delivered
  the max-value bid across relays; KEEP the existing delivered-count as a coverage check.
- **CB-preflight:** keep the honest `Inconclusive` stub — NOT shipping the partial preflight.

## Judgment calls for J (each changes what real runs report) — ANSWERED ABOVE
- **mux.routing (Tier-1):** when routing is unverifiable (CB debug logging off → `pubkeys_verified==0`),
  should it WARN or FAIL? WARN is honest ("couldn't verify") but if mux.routing stays Tier-1-must-pass, a
  WARN could fail runs that pass today. Options: (a) require CB debug logging on for mux scenarios (make it a
  precondition), (b) WARN + drop mux.routing from Tier-1 to "should", (c) FAIL and treat debug-logging-on as
  mandatory. Needs your call.
- **payload_matching:** compare per-(relay, slot) instead of collapsing by slot; a cross-relay hash
  disagreement should FAIL (or at least WARN loudly) naming the slot + the divergent relays. Confirm the
  desired severity. Also: should a delivered-but-not-on-chain slot (`missed>0`) affect the verdict?
- **relay_pipeline best-bid:** define what "aggregated bidding verified" means — compare bid values across
  relays per slot and assert the delivered payload matches the max-value bid? This is the most design-heavy
  of the three; may want its own brief.

## Proposed approach (TDD, mirrors the healthy `cb_metrics.rs` pattern)
For each check, in order (mux, payload, relay_pipeline):
1. **Extract the pure classifier** from the async fetch fn: `classify_<check>(already_fetched_data) ->
   CheckResult`. No behavior change in the extraction commit — the fetch fn calls the new pure fn; existing
   e2e behavior identical. (This is the ONLY safe-to-preview step; do it first, review, confirm green.)
2. **RED test** encoding the CORRECT contract (the false-green case asserts WARN/FAIL, not PASS).
3. **Fix** the classifier (pubkeys_verified gate / per-(relay,slot) compare / cross-relay value compare) to
   GREEN.
4. Full regression + a note in the run's report so the new WARN/FAIL is legible.

Each check is an independent slice; land + review one before the next. The extraction (step 1) is
mechanical and reviewable; steps 2-4 are where your judgment-call answers above get encoded.

## Status
- Confirmed + documented (this doc). Guard-branch Law 4 tests for chain_health + payload_matching already
  landed (`b0947b4`) — they cover the pre-fetch skip branches; the classifier extraction above is what makes
  the REAL boundaries testable.
- NOT started (verdict-changing). Supersedes the P2 plan's "Flagged for J" section.
