# cb-verify checks — the authoritative catalog

This is the per-check contract for `cb-verify`: what each check asserts, its tier, its
pass/warn/fail/skip conditions, and where its data comes from. It is meant to be read by both a
human and a CI/agent consumer of the verdict. Facts here are sourced from the code
(`src/checks/*.rs`, `src/report.rs`, `src/main.rs`); when in doubt, the code wins.

## The verdict model (read this first — it is load-bearing)

A run emits a `VerificationReport` (`src/report.rs`): an `enclave`, a `timestamp`, an
`observation_window`, an overall `result`, and a list of per-check `CheckResult`s. Each
`CheckResult` (`src/checks/mod.rs:26`) carries an `id`, a `tier` (`u8`), a `result` (serialized
name of `CheckStatus`: `PASS` / `FAIL` / `WARN` / `SKIP`), a `detail` string, and a `data` object.

**Tiers = severity contract:**

| Tier | Meaning | Effect on exit code / overall result |
|------|---------|--------------------------------------|
| 1 | **must** — a real pipeline invariant | A tier-1 `FAIL` fails the whole run |
| 2 | **should** — health signal | Never fails the run; annotative |
| 3 | **informational** | Never fails the run; annotative |

**The crucial contract — only a tier-1 FAIL is fatal.** `report::exit_code`
(`src/report.rs:131-145`) returns:

- `2` if **no** tier-1 checks ran at all (setup/discovery failure),
- `1` if **any** tier-1 check is `FAIL`,
- `0` otherwise.

The overall `report.result` is computed the same way (`src/main.rs:559-571`): it is `FAIL` iff some
tier-1 check is `FAIL`, else `PASS`. `WARN` and `SKIP` are **non-fatal and annotative** — they never
change the exit code and never change the overall result, at any tier.

**Consequence a consumer MUST internalize:** several checks that exist specifically to catch an
anomaly report that anomaly as `WARN`, not `FAIL` (relay equivocation in `payload_hash_match`,
unverifiable routing in `mux.routing`, a best-bid shortfall in `relay.best_bid`). A run in which
those fire **still exits 0**. A CI job or agent that trusts only the process exit code will call a
misbehaving pipeline green. **Parse the JSON and inspect each check's `result` field** — do not gate
on the exit code alone. If you want any of these `WARN`s to be fatal, that is a policy decision that
has to be made explicitly (see the P3 notes and Known gaps below); today they are not.

One escalation exists: the `cb_metrics` matrix checks are authored at tier 2 but are **promoted to
tier 1 when they FAIL** (`src/checks/cb_metrics.rs:636-641`), because a 5xx from a relay is a real
pipeline failure. So a matrix 5xx does gate the exit code even though the check's nominal tier is 2.

**Setup / preflight failures** (bad args, discovery failure, no beacon node, dead services, chain
never reached the window) short-circuit before the check phase and produce a single synthetic tier-1
`FAIL` check (`setup`), exit code `2` (`src/main.rs`, `make_error_report`).

## Data-source robustness

Checks split by where their evidence comes from, which determines how they behave when a service
dies mid-run:

- **CB-log-based / kurtosis-inspect-based — robust to relay death.** Evidence is in the CB
  container logs or `kurtosis enclave inspect`, which survive a relay crash: `cb_running`,
  `mux.routing`, and the *offered-bid* half of `relay.best_bid`.
- **CB Prometheus metrics — robust to relay death, but usually absent.** `cb_*_matrix`,
  `cb_v2_fallback`, `cb_relay_latency`. These SKIP wholesale unless CB was configured to expose
  metrics; the default kurtosis PBS mode does **not** set metrics config, so they SKIP by default.
- **Relay data-API-based — fragile.** If the relay dies before check time these `FAIL` or `SKIP`:
  `relay.builder_blocks_received`, `relay.payloads_delivered_multi`, `relay.mev_delivery_rate`,
  `relay.validator_registrations`, `payload_hash_match`, and the *delivered-value* half of
  `relay.best_bid`. `run_relay_checks` pings each relay first and emits a single `SKIP` per
  downstream check when all relays are unreachable (rather than a pile of request errors).
- **Beacon-API-based.** `chain_finality`, `sync_status`, `missed_slots`, plus the on-chain half of
  `relay.mev_delivery_rate` and `payload_hash_match`.

## Catalog

| id | tier | source | asserts |
|----|------|--------|---------|
| `chain_finality` | 1 | beacon API | finalized epoch ≥ 2 (conditionally run) |
| `sync_status` | 1 | beacon API | beacon node is done syncing |
| `cb_running` | 1 | kurtosis inspect | ≥1 commit-boost service is `running` |
| `missed_slots` | 2 | beacon API | missed-slot rate < 10% over the window |
| `relay.payloads_delivered_multi` | 1 | relay data API | ≥1 payload delivered across relays |
| `relay.builder_blocks_received` | 2 | relay data API | ≥1 builder block received by a relay |
| `relay.mev_delivery_rate` | 2 | relay data API + beacon | MEV-delivered block fraction ≥ threshold (0.30) |
| `relay.validator_registrations` | 3 | relay data API | validators are registered with the relay |
| `payload_hash_match` | 1 | relay data API + beacon | relay-delivered hashes match on-chain, no cross-relay conflict |
| `relay.best_bid` | 2 | CB logs + relay data API | CB delivered ≥ the best per-relay bid it was offered |
| `mux.routing` | 1 | CB logs | every checked getHeader routed per the `[[mux]]` config |
| `feature.timing_games` | 1 | CB logs | timing-games codepath fired (≥1 `TG:` debug line); config-gated |
| `feature.extra_validation` | 1 | CB logs | extra-validation codepath fired (≥1 parent-block fetch); config-gated |
| `feature.skip_sigverify` | 1 | CB logs | skip-sigverify enabled; **WARN-only** (not runtime-confirmable); config-gated |
| `cb_get_header_matrix` | 2 → 1 on FAIL | CB Prometheus | get_header status-code distribution healthy |
| `cb_register_validator_matrix` | 2 → 1 on FAIL | CB Prometheus | register_validator acceptance healthy |
| `cb_submit_blinded_block_matrix` | 2 → 1 on FAIL | CB Prometheus | ≥1 blinded-block delivery (200/202) |
| `cb_status_matrix` | 2 → 1 on FAIL | CB Prometheus | status endpoint answering 200 |
| `cb_v2_fallback` | 2 | CB Prometheus | no v2→v1 submitBlindedBlock fallbacks |
| `cb_relay_latency` | 2 | CB Prometheus | p95 relay latency < 500 ms |

Note: `relay.validator_registrations` (tier 3) is only added to the report when active validator
pubkeys were successfully fetched; if the pubkey fetch fails, the check is omitted entirely (it
does not even SKIP). `cb_v2_fallback` and `cb_relay_latency` are produced by the metrics phase but
were not part of the original catalog request; they are included here for completeness.

---

### `chain_finality` — tier 1 (beacon API)

Asserts the beacon chain has finalized past epoch 2. Source: `beacon.get_finalized_epoch()`.

- **PASS** — finalized epoch ≥ 2.
- **FAIL** — finalized epoch < 2, or the beacon query errored.
- **SKIP** — the surrounding `run_chain_health_checks` skips this check (and injects a tier-1 SKIP)
  when `--skip-finalization-check` is set, **or** when the observation window ends before slot 96
  (epoch 3), because the justification cascade has not had time to finalize epoch 2 yet.
- No WARN state.

### `sync_status` — tier 1 (beacon API)

Asserts the beacon node is not syncing. Source: `beacon.is_syncing()`.

- **PASS** — node reports not syncing.
- **FAIL** — node still syncing, or the query errored.
- No WARN/SKIP.

### `cb_running` — tier 1 (kurtosis inspect)

Asserts commit-boost is actually up. Source: `kurtosis enclave inspect <enclave>`, grepped
case-insensitively for the service pattern `commit-boost` and the word `running`.

- **PASS** — ≥1 matching service line also contains `running`.
- **FAIL** — matching services exist but none are `running`; **or** no matching services found;
  **or** the `kurtosis` CLI errored / returned non-zero.
- No WARN/SKIP.

### `missed_slots` — tier 2 (beacon API)

Asserts the miss rate over the window is under threshold. Source: `beacon.get_header(slot)` for each
slot in `[start, end)`; a `None` header or an error counts as missed. Threshold is hardcoded to
`0.10` by `run_chain_health_checks`.

- **PASS** — miss rate < 10%.
- **WARN** — miss rate ≥ 10%.
- **SKIP** — single-slot window (`start == end`): no interior slots to measure, so it deliberately
  SKIPs rather than PASS on zero data.
- **FAIL** — inverted range (`start > end`), treated as nonsense input.

### `relay.payloads_delivered_multi` — tier 1 (relay data API)

Asserts the MEV pipeline delivered at least one payload. Source: `get_payloads_delivered(start,end)`
across all live relays, unioned by slot.

- **PASS** — ≥1 delivered payload (across any relay).
- **FAIL** — zero delivered payloads across all relays.
- **SKIP** — all relays unreachable at check time (fragile-source SKIP from `run_relay_checks`).

### `relay.builder_blocks_received` — tier 2 (relay data API)

Asserts a relay received builder blocks. Source: `get_builder_blocks_received(slot)` sampled at ~10
slots across the window (the data API requires a filter param, so it samples). Aggregated across live
relays: PASS if any relay received blocks.

- **PASS** — ≥1 builder block received by at least one relay.
- **FAIL** — no builder blocks at any sampled slot on any relay.
- **SKIP** — all relays unreachable at check time.

### `relay.mev_delivery_rate` — tier 2 (relay data API + beacon)

Asserts a healthy fraction of on-chain blocks came from the relay. Source: delivered block hashes
from the first relay whose data API responds, intersected with on-chain block hashes
(`beacon.get_block_hash(slot)`) over the window. Threshold is `--mev-threshold` (default `0.30`).

- **PASS** — `mev_blocks / total_blocks` ≥ threshold.
- **WARN** — rate below threshold.
- **FAIL** — no proposed (on-chain) blocks found in the window (`total_blocks == 0`).
- **SKIP** — no relay supports the data API, or all relays unreachable.

### `relay.validator_registrations` — tier 3 (relay data API)

Asserts validators are registered with the relay. Source: `GET
/relay/v1/data/validator_registration?pubkey=…` for each active validator pubkey, per live relay,
aggregated to the worst status.

- **PASS** — all pubkeys registered.
- **WARN** — some registered, some missing.
- **FAIL** — none registered (`0/total`).
- **SKIP** — pubkey list empty, or all relays unreachable. If the upstream pubkey fetch failed
  entirely, the check is **omitted** from the report rather than SKIPped.

### `payload_hash_match` — tier 1 (relay data API + beacon)

Cross-checks each relay's delivered `block_hash` against the on-chain hash per slot, and detects
cross-relay disagreement. Source: per-(relay, slot) delivered hashes (kept un-deduped, on purpose) +
`beacon.get_block_hash(slot)`.

- **PASS** — every observed slot has a relay hash matching the on-chain hash, and no slot has two
  relays reporting divergent hashes.
- **WARN** — any slot where no relay hash matched the on-chain hash (`mismatched > 0`, possible
  reorg) **or** any slot with a cross-relay hash conflict (`cross_relay_conflicts > 0`, relay
  equivocation). The offending slots and relays are named in `data`.
- **SKIP** — no delivered payloads to compare (this check explicitly defers the "was anything
  delivered" signal to `relay.payloads_delivered_multi` rather than PASS on zero comparisons).
- `missed` (a delivered slot with no on-chain block) is counted but does **not** downgrade the
  verdict — informational only.

### `relay.best_bid` — tier 2 (CB logs + relay data API)

Asserts CB delivered at least the best bid it was actually offered across relays (aggregated
bidding). Offered bids come from CB's own `received new header` getHeader log events (`relay_id`,
`slot`, `value_eth` parsed decimal→wei exactly, no float) — the bids CB itself compared. Delivered
values come from the relay data API (the winning payload's value). Comparison is exact wei vs exact
wei.

- **SKIP** — fewer than 2 relays: no cross-relay aggregation is possible to verify.
- **WARN (not exercised)** — no slot had ≥2 distinct relays offering bids; aggregation never
  happened, so nothing is asserted.
- **WARN (not verified)** — competitive slots exist but none had a delivered payload to compare
  against (out of window / missed slot); the Law-3 guard against greening having compared nothing.
- **WARN (suboptimal)** — a verified competitive slot delivered **less** than the best offered bid
  (value left on the table — late, rejected, or ineligible header). The slots are named in `data`.
- **PASS** — ≥1 verified competitive slot, and every one delivered ≥ its best offered bid.

### `mux.routing` — tier 1 (CB logs)

Asserts CB routed each getHeader to the mux/relay the `[[mux]]` config specifies. Only runs when
`--config` was given and the CB config contains `[[mux]]` sections. Source: CB PBS container logs
(`using mux config` DEBUG events carry `mux_id` + `validator`; a routing decision is only "verified"
when a known pubkey is seen with its mux_id).

- **PASS** — ≥1 routing decision was actually checked and all checked decisions routed to the
  expected mux.
- **FAIL** — a checked decision routed a pubkey to the wrong mux/relay (misrouting). Details name
  the pubkey, the routed vs expected mux and relay.
- **WARN (no events)** — no mux-related log lines at all; routing could not be observed.
- **WARN (nothing verified)** — mux log events exist but zero routing decisions were verifiable
  (`routing_decisions_verified == 0`), typically because CB debug logging is off so there is no
  `using mux config` line. Requires `[logs.stdout] level = "debug"`.
- **SKIP** — no `[[mux]]` entries to verify. (A config that fails to parse produces a tier-1 FAIL
  from `main.rs`, not a SKIP.)

### `feature.*` — tier 1 (CB logs), Law-3 feature-fired assertions

One check per CB feature the `--config` enables, proving the feature's codepath actually fired at
runtime (not just that generic health passed). Source: CB PBS debug logs (the toggle scenarios set
`[logs.stdout] level = "debug"`). Detected by scanning the CB config template for `<key> = true`.
Each is emitted **only when its feature is enabled** — an off feature produces no check at all.

- **`feature.timing_games`** (`enable_timing_games`) — **PASS** on ≥1 `TG:` debug line
  (`send_timed_get_header`), else **WARN** (enabled but unobserved — maybe no getHeader in the window).
- **`feature.extra_validation`** (`extra_validation_enabled`) — **PASS** on ≥1 `fetched parent block`
  / `fetching parent block` line, else **WARN**.
- **`feature.skip_sigverify`** (`skip_sigverify`) — **always WARN**. This is a *negative* codepath
  (sigverify is simply not called; no success log or metric), indistinguishable from OFF on the happy
  path. Honestly reports "not runtime-confirmable" rather than a false green. Confirming it needs a
  bad-signature-injecting relay in the helix mock.

A marker feature enabled-but-unobserved is WARN, never FAIL (no-false-red — the same discipline as
`mux.routing`). All three are non-fatal (only a tier-1 FAIL fails the run).

### `cb_*_matrix` — tier 2, escalates to tier 1 on FAIL (CB Prometheus)

Four checks, one per endpoint, built from CB's status-code counters
`cb_pbs_relay_status_code_total` (codes CB received from relays, the source of truth) and
`cb_pbs_beacon_node_status_code_total` (codes CB returned to the CL, surfaced for cross-boundary
diagnosis). Codes bucket into `200 / 202 / 204 / 4xx / 5xx / timeout / other`. **`timeout` is CB's
synthetic code 555** (`TIMEOUT_ERROR_CODE`) — not a relay-served status but CB cancelling its own
request at its deadline; it must never count as relay 5xx (live-confirmed 2026-08-03: timing-games
produced 42% 555s with ZERO real relay 5xx, and the old bucketing tier-1-failed the run). Metrics are
fetched over HTTP, falling back to `kurtosis exec`; if neither works (the usual case — default
kurtosis PBS mode sets no metrics config), **all** matrix checks plus `cb_v2_fallback` and
`cb_relay_latency` SKIP.

Shared rules across all four: **relay-side 5xx FAILs when it exceeds 25% of COMPLETED responses**
(timeouts excluded from the denominator, so a real error storm still fails amid heavy timeout
polling); at or below the rate it's a transient-warmup WARN, promoted to FAIL under `--strict`.
**CB-deadline timeouts (555) above 25% → WARN, never FAIL — not even under `--strict`** (client-side
deadline policy, e.g. timing-games cancelling late polls by design, or a slow relay). Any matrix FAIL
is escalated from tier 2 to tier 1 so it gates the exit code. **No samples for the endpoint → SKIP.**

- **`cb_get_header_matrix`** — PASS if any 200 (bids delivered, timeout count noted); WARN if only
  204s (relay alive, no bid — promoted to **FAIL under `--strict`**); FAIL if only 4xx.
- **`cb_register_validator_matrix`** — PASS if 200s and zero 4xx (100% accepted); WARN if a mix of
  200 and 4xx (some batches rejected — normal early on; the beacon-side 502 translation is surfaced);
  FAIL if only 4xx; SKIP if no registrations observed; FAIL on any 5xx.
- **`cb_submit_blinded_block_matrix`** — PASS if any (200 + 202) deliveries (200 = v1 path, 202 = v2
  path); WARN if zero deliveries (proposer never chose a builder block — promoted to **FAIL under
  `--strict`**); FAIL on any 5xx.
- **`cb_status_matrix`** — PASS if any 200; SKIP if no 200s; FAIL on any 5xx.

### `cb_v2_fallback` — tier 2 (CB Prometheus)

Asserts relays support submitBlindedBlockV2. Source:
`cb_pbs_submit_block_v2_fallback_to_v1_total`. A missing counter = never incremented = zero
fallbacks.

- **PASS** — total fallbacks == 0 (includes the missing-counter case).
- **WARN** — any v2→v1 fallback; a relay is behind on the builder-specs v2 upgrade. Never FAIL (v1
  still works), unaffected by `--strict`.

### `cb_relay_latency` — tier 2 (CB Prometheus)

Asserts p95 relay latency under threshold. Source: the `cb_pbs_relay_latency` histogram, aggregated
across all `{endpoint, relay_id}` dimensions; standard `histogram_quantile` at q=0.95. Threshold is
hardcoded to 500 ms.

- **PASS** — p95 < 500 ms.
- **WARN** — p95 ≥ 500 ms.
- **SKIP** — histogram not exposed, zero observations, or degenerate buckets.

---

## The P3 trust-fix notes (the load-bearing WHY behind three WARN gates)

Three checks were rewritten (plan: `docs/plans/P3-check-trustworthiness.md`, North-Star Law 3 "a
harness that lies green is worse than an ugly one") to kill a **false green** — a PASS reported while
the check had verified nothing. Each now WARNs instead of passing-on-nothing:

- **`mux.routing`** — the old pass-gate keyed on `total_events` (raw log lines) and PASSed whenever
  there were no violations, even with **zero** parseable routing decisions (CB debug logging off). It
  reported "all N routing decisions verified" where N was log lines, not decisions. Fixed to gate on
  `routing_decisions_verified`; zero verified → WARN, and debug logging is required for mux scenarios.
- **`payload_hash_match`** — the old code was a first-wins union by slot
  (`by_slot.entry(slot).or_insert(hash)`) that kept only the first relay's hash and **dropped
  cross-relay disagreement** before the on-chain compare, making the verdict order-dependent (honest
  relay first → false PASS). Fixed to keep every (relay, slot) hash and detect divergence → WARN,
  naming the slot and relays.
- **`relay.best_bid`** — the old check was a first-wins union that counted distinct delivered slots
  and never compared bid **values** across relays, so one delivering relay scored identically to
  genuine two-relay aggregation. A first rewrite sourced offered bids from
  `get_builder_blocks_received`, but adversarial review found that source unsound (it includes builder
  submissions that failed simulation and were never offered to the proposer — a false alarm on correct
  runs), so it was backed out. The shipped version sources offered bids from CB's `received new
  header` log events (what CB actually compared) and WARNs whenever aggregation was not exercised or no
  competitive slot could be verified, rather than PASS.

All three land as `WARN`, which per the verdict model is **non-fatal** — see the consumer caveat in
the intro.

## Known gaps and caveats (factual, from the code)

- **Feature-fired assertions (Law 3) — mostly closed.** North-Star Law 3 wants every scenario to
  positively assert its feature's codepath fired. Now five checks do: `mux.routing`, `relay.best_bid`,
  and the config-gated `feature.timing_games` (≥1 `TG:` debug line), `feature.extra_validation` (≥1
  parent-block fetch log). The residual gap is **`skip_sigverify`**: it is a *negative* codepath
  (signature verification simply not called, with no success log or metric), indistinguishable from
  OFF on the happy path with a valid-signature mock relay. `feature.skip_sigverify` therefore reports
  an honest **WARN** ("not runtime-confirmable") rather than a false green. Closing it fully needs a
  bad-signature-injecting relay in the helix mock (then: ON delivers the bad-sig bid, OFF rejects it).
  A marker feature that is enabled but unobserved WARNs (could be no getHeader in the window), never
  FAILs — the no-false-red discipline.
- **`relay.best_bid` is inert or degenerate in common setups.** It SKIPs any single-relay run (no
  aggregation to verify), and in mux mode — where each mux typically points a validator at exactly one
  relay — no slot sees ≥2 relays competing, so it WARNs "aggregation not exercised" rather than
  verifying anything. With an identical two-relay setup (e.g. two helix instances serving the same
  bid) the comparison is technically competitive but degenerate.
- **The `cb_*_matrix` counters are cumulative, not windowed.** The H2 fix made the 5xx verdict
  rate-based (FAIL only above 25% of completed responses; below = transient-warmup WARN), and code 555
  now buckets as `timeout` (WARN-only), so neither a warmup blip nor a designed deadline-cancellation
  fails a run anymore. But the counters still cover the container's whole life, not the observation
  window — a sustained pre-window error burst can still dominate the rate. True windowing (delta
  against a baseline scrape, as `--live-metrics` already takes) remains future work.
- **A real anomaly can still exit 0.** As stated in the intro: relay equivocation, unverifiable
  routing, and best-bid shortfall are all `WARN`. Gate on the JSON `result` per check, not the exit
  code, if you care about these.
- **Metrics are usually absent.** The default kurtosis PBS mode does not configure CB metrics, so the
  six Prometheus-based checks SKIP unless metrics are explicitly enabled.
- **`payload_hash_match` leniency (left as-is under P3 scope).** A delivered-but-not-on-chain slot
  (`missed`) is indistinguishable from a transient beacon error and never downgrades the verdict; a
  hash mismatch is WARN, not FAIL.
</content>
</invoke>
