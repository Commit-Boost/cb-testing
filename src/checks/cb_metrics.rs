//! Commit-Boost Prometheus metrics verification checks.
//!
//! Replaces the older narrow checks (5xx-only, 200-only) with a status-code
//! matrix that captures the full distribution of HTTP codes CB emits
//! per-endpoint, per-relay. This is how you actually debug a PBS pipeline:
//! by knowing whether you're getting 204s (no bids) versus 500s (broken
//! relay), not just "some errors happened".
//!
//! # Spec-defined codes per endpoint
//!
//! | Endpoint               | Success codes | Semantics                                         |
//! |------------------------|---------------|---------------------------------------------------|
//! | `get_header`           | 200           | Relay delivered a bid                             |
//! |                        | 204           | Valid request, no bid this slot (NORMAL)          |
//! | `register_validator`   | 200           | Registrations accepted                            |
//! | `submit_blinded_block` | 200           | v1 path: CB returned payload, CL publishes block  |
//! |                        | 202           | v2 path: relay publishes unblinded block itself   |
//! | `status`, `reload`     | 200           | Endpoint alive / reload succeeded                 |
//!
//! 4xx = client bug. 5xx = relay or CB internal failure. 555 = NOT an HTTP
//! response at all: CB's synthetic client-side timeout marker (commit-boost
//! `constants.rs` `TIMEOUT_ERROR_CODE`), incremented when CB cancels its own
//! request at its deadline. It gets its own `timeout` bucket — counting it as
//! relay 5xx made the timing-games scenario (which cancels late polls at the
//! 400ms budget BY DESIGN) tier-1-fail a run whose relays served zero errors
//! (live-confirmed 2026-08-03: raw counter 200x48/204x6/400x4/555x42, zero
//! real 5xx in helix logs).
//!
//! # Output shape
//!
//! Each check returns ONE `CheckResult` per endpoint with per-relay counts
//! nested under `data.by_relay`. See [`EndpointStats`] for the schema.

use std::collections::BTreeMap;

use prometheus_parse::Scrape;

use crate::checks::{CheckResult, CheckStatus};
use crate::metrics;

const METRICS_PORT: u16 = 9090;

/// A relay/CB-to-relay 5xx FRACTION above this fails the matrix check; at or below
/// it, a nonzero 5xx count is a transient WARN (warmup noise). See classify_endpoint
/// (the H2 warmup-5xx false-red fix). `--strict` ignores this and fails on any 5xx.
const MAX_5XX_RATE: f64 = 0.25;

/// A CB client-side timeout (code 555) FRACTION above this WARNs — high timeout
/// rates are either an aggressive timing config (timing-games cancels late polls
/// at its budget by design) or a slow relay, both worth surfacing. Never FAIL:
/// a 555 is CB's own deadline policy firing, not a relay-served error.
const MAX_TIMEOUT_RATE: f64 = 0.25;

/// Status-code counts for a single endpoint, split by observation side.
///
/// CB emits the same HTTP-level event at two layers:
///
/// - `cb_pbs_relay_status_code_total`   -- codes CB **received from relays**
/// - `cb_pbs_beacon_node_status_code_total` -- codes CB **returned to the CL**
///
/// These are NOT duplicates. They're the same logical event observed on
/// opposite edges of the PBS proxy. For register_validator in particular,
/// CB translates relay 4xx into 502 toward the CL, so summing both sides
/// double-counts. Keep them separate, display separately, and let the
/// reader diagnose across the boundary.
///
/// JSON schema when serialized into a `CheckResult.data` field:
///
/// ```json
/// {
///   "endpoint": "register_validator",
///   "relay_side": {
///     "totals":   {"200": 26, "4xx": 19, "5xx": 0, ...},
///     "by_relay": {"mev_relay_0": {"200": 26, "4xx": 19, ...}}
///   },
///   "beacon_side": {
///     "totals": {"200": 26, "5xx": 19, ...}
///   }
/// }
/// ```
#[derive(Debug, Default, Clone)]
pub struct EndpointStats {
    /// What CB received from relays, bucketed by relay_id.
    pub relay_totals: BTreeMap<String, f64>,
    pub relay_by_id: BTreeMap<String, BTreeMap<String, f64>>,
    /// What CB returned to the CL (no relay_id label; aggregated across relays
    /// by CB's response handling).
    pub beacon_totals: BTreeMap<String, f64>,
}

impl EndpointStats {
    fn add_relay(&mut self, relay_id: &str, code: &str, count: f64) {
        let bucket = bucket_code(code);
        *self.relay_totals.entry(bucket.clone()).or_default() += count;
        *self
            .relay_by_id
            .entry(relay_id.to_string())
            .or_default()
            .entry(bucket)
            .or_default() += count;
    }

    fn add_beacon(&mut self, code: &str, count: f64) {
        *self.beacon_totals.entry(bucket_code(code)).or_default() += count;
    }

    fn relay_get(&self, bucket: &str) -> f64 {
        self.relay_totals.get(bucket).copied().unwrap_or(0.0)
    }

    fn beacon_get(&self, bucket: &str) -> f64 {
        self.beacon_totals.get(bucket).copied().unwrap_or(0.0)
    }

    fn has_samples(&self) -> bool {
        !self.relay_totals.is_empty() || !self.beacon_totals.is_empty()
    }

    fn to_json(&self, endpoint: &str) -> serde_json::Value {
        fn codes_map(m: &BTreeMap<String, f64>) -> serde_json::Map<String, serde_json::Value> {
            m.iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(*v as u64)))
                .collect()
        }
        let relay_by_id: serde_json::Map<String, serde_json::Value> = self
            .relay_by_id
            .iter()
            .map(|(rid, codes)| (rid.clone(), serde_json::Value::Object(codes_map(codes))))
            .collect();
        serde_json::json!({
            "endpoint": endpoint,
            "relay_side": {
                "totals":   codes_map(&self.relay_totals),
                "by_relay": relay_by_id,
            },
            "beacon_side": {
                "totals": codes_map(&self.beacon_totals),
            }
        })
    }
}

/// Bucket a raw HTTP code string into the reporting categories.
///
/// Exact codes we care about (200, 202, 204) pass through. 2xx other than
/// those is rare but real (e.g. 201) and gets bucketed as `other` to avoid
/// silently counting it as success.
fn bucket_code(code: &str) -> String {
    match code {
        "200" | "202" | "204" => code.to_string(),
        // CB's synthetic client-side timeout marker (TIMEOUT_ERROR_CODE = 555).
        // Not a relay-served status — MUST NOT land in the 5xx bucket, or a
        // designed deadline-cancellation counts as a relay failure.
        "555" => "timeout".to_string(),
        // CB's synthetic transport-error marker (TRANSPORT_ERROR_CODE = 556,
        // introduced with WS get_header streaming: connect refused / dns / tls
        // / stream broke mid-window). Also client-observed, not relay-served —
        // must not count toward the relay 5xx error rate. Relay reachability
        // has its own tier-1 owner (the relay_pipeline death checks).
        "556" => "transport".to_string(),
        c if c.starts_with('4') && c.len() == 3 => "4xx".to_string(),
        c if c.starts_with('5') && c.len() == 3 => "5xx".to_string(),
        _ => "other".to_string(),
    }
}

/// Collect status-code counts for a given endpoint from a scrape.
///
/// Populates both sides of the CB metric model independently (see
/// [`EndpointStats`] doc for why we keep them separate).
pub fn collect_endpoint_stats(scrape: &Scrape, endpoint: &str) -> EndpointStats {
    let mut stats = EndpointStats::default();

    for s in &scrape.samples {
        let is_beacon_side = s.metric == "cb_pbs_beacon_node_status_code_total";
        let is_relay_side = s.metric == "cb_pbs_relay_status_code_total";
        if !is_beacon_side && !is_relay_side {
            continue;
        }
        if s.labels.get("endpoint") != Some(endpoint) {
            continue;
        }
        let code = match s.labels.get("http_status_code") {
            Some(c) => c.to_string(),
            None => continue,
        };
        let count = match &s.value {
            prometheus_parse::Value::Counter(v)
            | prometheus_parse::Value::Gauge(v)
            | prometheus_parse::Value::Untyped(v) => *v,
            _ => continue,
        };
        if is_relay_side {
            let relay_id = s
                .labels
                .get("relay_id")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            stats.add_relay(&relay_id, &code, count);
        } else {
            stats.add_beacon(&code, count);
        }
    }

    stats
}

/// Classify an endpoint's status-code distribution into a `CheckResult`.
///
/// Uses the relay-side counts as the source of truth for the pipeline
/// (what the relay actually saw), and surfaces the beacon-side view in
/// the detail string so mismatches are visible. Most endpoints agree on
/// both sides; `register_validator` is the notable exception where
/// relay-side 4xx become beacon-side 5xx (BAD_GATEWAY).
///
/// # `strict` mode
///
/// When `strict == true`, the following conditions that would normally WARN
/// are promoted to FAIL:
///
/// - `submit_blinded_block` with zero (200 + 202) deliveries -- proposer
///   never chose a builder block. On devnet this is often normal (proposer
///   win rate is tiny); in CI it means the pipeline is dead.
/// - `get_header` with zero 200s but some 204s -- relay responded but
///   never delivered a bid. On devnet this happens if the local builder
///   can't produce; in CI we want this to fail.
///
/// All 5xx conditions on relay-side FAIL regardless of `strict`. A 5xx on
/// the beacon side that corresponds to relay 4xx (register_validator) is
/// WARN at relay-side 4xx rate, since the translation is expected behavior
/// when a relay rejects a registration.
pub fn classify_endpoint(endpoint: &str, stats: &EndpointStats, strict: bool) -> CheckResult {
    let id = format!("cb_{endpoint}_matrix");
    let tier = 2u8;
    let data = stats.to_json(endpoint);

    if !stats.has_samples() {
        return CheckResult::skip(id, tier, format!("No {endpoint} samples in scrape"))
            .with_data(data);
    }

    // Use relay-side as authoritative for get_header / submit_blinded_block
    // (those flows originate upstream of CB). For register_validator we
    // also pay attention because CB aggregates multiple registrations into
    // one relay call, so relay counts != beacon counts.
    let r200 = stats.relay_get("200");
    let r202 = stats.relay_get("202");
    let r204 = stats.relay_get("204");
    let r4xx = stats.relay_get("4xx");
    let r5xx = stats.relay_get("5xx");
    let rtimeout = stats.relay_get("timeout");
    let rtransport = stats.relay_get("transport");
    let b5xx = stats.beacon_get("5xx");

    // Relay 5xx handling (H2 fix). These are absolute CUMULATIVE counters that
    // include the pre/early-window warmup phase, where a handful of 5xx are normal
    // (relays/builders not yet ready). FAILing on any 5xx > 0 made a genuinely
    // healthy run red. Classify on the 5xx RATE instead: a materially broken
    // relay/CB-to-relay link produces a high fraction of 5xx (FAIL); a few 5xx
    // against many good requests is transient warmup (WARN, surfaced but non-fatal).
    // `--strict` still promotes the transient case to FAIL for zero-tolerance CI.
    if r5xx > 0.0 {
        // Timeouts (555) are deliberately EXCLUDED from this denominator: the
        // 5xx rate is "fraction of COMPLETED relay responses that were errors",
        // so a real 5xx storm still FAILs even amid heavy timeout polling.
        let total = r200 + r202 + r204 + r4xx + r5xx;
        let rate = if total > 0.0 { r5xx / total } else { 1.0 };
        let pct = rate * 100.0;
        if rate > MAX_5XX_RATE || strict {
            return CheckResult::fail(
                id,
                tier,
                format!(
                    "{endpoint}: {r5xx:.0}/{total:.0} 5xx ({pct:.1}%) from relay(s) -- relay or \
                     CB-to-relay failure (>{:.0}% threshold{})",
                    MAX_5XX_RATE * 100.0,
                    if strict { ", --strict" } else { "" }
                ),
            )
            .with_data(data);
        }
        return CheckResult::warn(
            id,
            tier,
            format!(
                "{endpoint}: {r5xx:.0}/{total:.0} transient 5xx ({pct:.1}%) -- likely warmup, below \
                 the {:.0}% FAIL threshold",
                MAX_5XX_RATE * 100.0
            ),
        )
        .with_data(data);
    }

    // CB client-side synthetic codes: 555 (deadline timeout) and 556 (WS
    // transport error). A high rate of either is surfaced as WARN — 555 means
    // an aggressive timing config cancelling late polls by design
    // (timing-games) or a slow relay; 556 means the stream relay is
    // unreachable/breaking (relay reachability's FAIL owner is the tier-1
    // relay_pipeline death check, not this matrix). Never FAIL here, and never
    // under --strict either: neither is a relay-served pipeline error. Below
    // the threshold, fall through to normal classification (the counts stay
    // visible in `data`).
    let rsynthetic = rtimeout + rtransport;
    if rsynthetic > 0.0 {
        let completed = r200 + r202 + r204 + r4xx + r5xx;
        let rate = rsynthetic / (completed + rsynthetic);
        if rate > MAX_TIMEOUT_RATE {
            let pct = rate * 100.0;
            return CheckResult::warn(
                id,
                tier,
                format!(
                    "{endpoint}: {rsynthetic:.0}/{:.0} CB client-side failures ({pct:.1}%: \
                     {rtimeout:.0} deadline timeouts (555), {rtransport:.0} ws transport errors \
                     (556)) -- not relay-served errors ({r200:.0} bids still delivered). 555 = \
                     aggressive timing config or slow relay; 556 = stream relay \
                     unreachable/breaking (relay death is the tier-1 relay checks' call)",
                    completed + rsynthetic
                ),
            )
            .with_data(data);
        }
    }

    match endpoint {
        "get_header" => {
            if r200 > 0.0 {
                let mut timeout_note = if rtimeout > 0.0 {
                    format!(", {rtimeout:.0} CB-deadline timeout (555)")
                } else {
                    String::new()
                };
                if rtransport > 0.0 {
                    timeout_note.push_str(&format!(", {rtransport:.0} ws transport error (556)"));
                }
                CheckResult::pass(
                    id,
                    tier,
                    format!(
                        "get_header: {r200:.0} bids delivered, {r204:.0} no-bid (204), {r4xx:.0} 4xx{timeout_note}"
                    ),
                )
                .with_data(data)
            } else if r204 > 0.0 {
                let msg = format!(
                    "get_header: 0 bids delivered, {r204:.0} no-bid (204). Builder never produced an acceptable bid. Pass --strict to treat as failure"
                );
                if strict {
                    CheckResult::fail(id, tier, msg.replace("Pass --strict ", "(--strict) "))
                        .with_data(data)
                } else {
                    CheckResult::warn(id, tier, msg).with_data(data)
                }
            } else {
                CheckResult::fail(
                    id,
                    tier,
                    format!(
                        "get_header: only 4xx responses ({r4xx:.0}); proposer requests malformed?"
                    ),
                )
                .with_data(data)
            }
        }
        "register_validator" => {
            // For register_validator, a relay 4xx means the relay rejected
            // that batch. CB then returns 502 to the CL. Both are expected
            // in concert -- beacon-side 5xx should roughly equal relay-side
            // 4xx (modulo batch semantics). Surface the acceptance rate,
            // and WARN (not FAIL) when rejections dominate.
            let relay_attempts = r200 + r4xx;
            let acceptance = if relay_attempts > 0.0 {
                100.0 * r200 / relay_attempts
            } else {
                0.0
            };
            let beacon_translation_note = if b5xx > 0.0 {
                format!(" (CB returned {b5xx:.0} 502 to CL)")
            } else {
                String::new()
            };

            if r200 > 0.0 && r4xx == 0.0 {
                CheckResult::pass(
                    id,
                    tier,
                    format!(
                        "register_validator: {r200:.0}/{relay_attempts:.0} accepted (100%){beacon_translation_note}"
                    ),
                )
                .with_data(data)
            } else if r200 > 0.0 && r4xx > 0.0 {
                // Relay rejected some batches but accepted others. On a
                // devnet this is typical during the first couple epochs
                // before validators are live. WARN, not FAIL.
                CheckResult::warn(
                    id,
                    tier,
                    format!(
                        "register_validator: {r200:.0}/{relay_attempts:.0} accepted ({acceptance:.0}%), {r4xx:.0} rejected by relay{beacon_translation_note}"
                    ),
                )
                .with_data(data)
            } else if r4xx > 0.0 {
                CheckResult::fail(
                    id,
                    tier,
                    format!(
                        "register_validator: 0 accepted, {r4xx:.0} 4xx from relay -- check CL signing / relay registration policy{beacon_translation_note}"
                    ),
                )
                .with_data(data)
            } else {
                CheckResult::skip(id, tier, "register_validator: no registrations observed")
                    .with_data(data)
            }
        }
        "submit_blinded_block" => {
            let delivered = r200 + r202;
            if delivered > 0.0 {
                CheckResult::pass(
                    id,
                    tier,
                    format!(
                        "submit_blinded_block: {r200:.0} v1 (200), {r202:.0} v2 (202); {delivered:.0} total deliveries, {r4xx:.0} 4xx"
                    ),
                )
                .with_data(data)
            } else if r4xx > 0.0 {
                // The proposer DID choose builder blocks — the relay refused
                // them. Diagnosing this as "proposer never chose" (which the
                // old 200+202==0 branch did) sends an operator to the wrong
                // component entirely. Found live on the nethermind+prysm pair
                // (2026-08-04): 26 blinded blocks forwarded, 26 relay 4xx.
                CheckResult::fail(
                    id,
                    tier,
                    format!(
                        "submit_blinded_block: the relay REJECTED all {r4xx:.0} blinded block(s)                          the proposer submitted (0 deliveries, {r4xx:.0} 4xx from relay) -- the                          proposer DID choose builder blocks; the break is relay-side (block                          invalid/late/unsigned as the relay sees it), not proposer-side"
                    ),
                )
                .with_data(data)
            } else {
                let msg = "submit_blinded_block: 0 deliveries and 0 submissions -- the proposer never chose a builder block. Pass --strict to treat as failure".to_string();
                if strict {
                    CheckResult::fail(id, tier, msg.replace("Pass --strict ", "(--strict) "))
                        .with_data(data)
                } else {
                    CheckResult::warn(id, tier, msg).with_data(data)
                }
            }
        }
        "status" | "reload" => {
            if r200 > 0.0 {
                CheckResult::pass(id, tier, format!("{endpoint}: {r200:.0} 200 responses"))
                    .with_data(data)
            } else {
                CheckResult::skip(id, tier, format!("{endpoint}: no 200 responses")).with_data(data)
            }
        }
        _ => CheckResult::skip(id, tier, format!("Unknown endpoint: {endpoint}")).with_data(data),
    }
}

/// The EXPOSED name of CB's v2-unsupported counter.
///
/// Note the doubled `pbs_`: the PBS registry is `Registry::new_custom(Some(
/// "cb_pbs"))`, which prefixes every metric, and this counter is *registered*
/// as `pbs_submit_block_v2_unsupported_total` - unlike its siblings, which are
/// registered bare (`relay_status_code_total` -> `cb_pbs_relay_status_code_total`).
const V2_UNSUPPORTED_METRIC: &str = "cb_pbs_pbs_submit_block_v2_unsupported_total";

/// Check the v2-unsupported counter: v2 submissions a relay could not serve.
///
/// `pbs_submit_block_v2_unsupported_total{relay_id}` ticks when a relay 404s
/// the v2 `submit_block` route. CB deliberately does NOT downgrade to v1 there
/// (in v2 the relay publishes the block after an empty 202, so a v1 payload
/// would be silently dropped by the beacon node) — it fails the submission.
///
/// This is FATAL to the MEV pipeline for any CL that submits via v2: every
/// builder block that proposer chooses is lost, and the slot is typically
/// missed. Found live on nethermind+prysm (2026-08-04): prysm submits to
/// `/eth/v2/builder/blinded_blocks` at ~256ms into the slot, helix 404s the v2
/// route, CB returns 502, and 11 v2-unsupported events lined up with 11 missed
/// slots. Lighthouse never triggers it because it submits via v1 — the exact
/// class of client-pair-specific break Law 7 exists to surface.
///
/// ROOT CAUSE THAT TIME WAS OUR OWN CONFIG, not a helix limitation: helix has a
/// `GetPayloadV2` route and our generated `router_config.enabled_routes` listed
/// only `GetPayload`. Read a 404 on the v2 route as "v2 is DISABLED at the
/// relay" first, and "the relay cannot do v2" only after the route list has
/// actually been checked.
///
/// Missing counter == zero == PASS (Prometheus omits never-incremented
/// families). Tier 2 -> escalated to tier 1 on FAIL by the caller, like the
/// matrix checks: a relay that cannot serve the proposer's submissions is a
/// real pipeline failure, not an annotation.
///
/// **The metric name has a doubled `pbs_`** (see [`V2_UNSUPPORTED_METRIC`]).
/// Matching CB's *registered* name instead of its *exposed* name made this
/// check structurally unable to fire: it reported PASS on a run where CB had
/// logged 11 v2-unsupported events, and that false PASS was read as evidence
/// that a relay-route fix had worked. Verify metric names against a real
/// scrape, never against the registration constant in CB's source.
pub fn check_v2_unsupported(scrape: &Scrape) -> CheckResult {
    let id = "cb_relay_v2_unsupported";

    let mut by_relay: BTreeMap<String, f64> = BTreeMap::new();
    for s in &scrape.samples {
        if s.metric != V2_UNSUPPORTED_METRIC {
            continue;
        }
        let relay = s
            .labels
            .get("relay_id")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let v = match &s.value {
            prometheus_parse::Value::Counter(v)
            | prometheus_parse::Value::Gauge(v)
            | prometheus_parse::Value::Untyped(v) => *v,
            _ => continue,
        };
        *by_relay.entry(relay).or_default() += v;
    }

    let total: f64 = by_relay.values().sum();
    let data = serde_json::json!({ "by_relay": by_relay, "total": total as u64 });

    if total == 0.0 {
        CheckResult::pass(id, 2, "No v2-unsupported submissions").with_data(data)
    } else {
        let relays: Vec<&str> = by_relay.keys().map(|s| s.as_str()).collect();
        CheckResult::fail(
            id,
            2,
            format!(
                "{total:.0} v2 submit_block(s) LOST: relay(s) {} 404 the v2 route and CB will not                  downgrade to v1 (v2 = the relay publishes the block; a v1 payload would be                  silently dropped). Every builder block the proposer chose was lost -- expect                  missed slots. CHECK THE RELAY ROUTE CONFIG FIRST: helix has a GetPayloadV2                  route that must be listed in router_config.enabled_routes -- a 404 here                  usually means v2 is merely DISABLED, not unsupported",
                relays.join(", ")
            ),
        )
        .with_data(data)
    }
}

/// Check the v2 -> v1 fallback counter.
///
/// **This check is INERT and reports SKIP.** It reads
/// `cb_pbs_submit_block_v2_fallback_to_v1_total`, and no such counter exists in
/// commit-boost: nothing named `*fallback*` is registered anywhere in
/// `crates/pbs/src/metrics.rs`. Because the check treated "counter absent" as
/// "zero fallbacks == PASS", it returned PASS on every run since it was written:
/// a check that cannot fail, which is worse than no check, and which also
/// claimed "relays support v2" on a run where the relay was 404ing v2.
///
/// It is kept (rather than deleted) as a SKIP so the id stays in the report and
/// the reason travels with it. CB does NOT downgrade v2 -> v1 on a 404 by
/// design (v2 semantics: the relay publishes the block, so a v1 payload would
/// be silently dropped) - it fails loud and increments the v2-UNSUPPORTED
/// counter instead, which is what [`check_v2_unsupported`] reads. If a real
/// fallback counter ever lands, restore the logic and re-point the name.
pub fn check_v2_fallback(scrape: &Scrape) -> CheckResult {
    let _ = scrape;
    CheckResult::skip(
        "cb_v2_fallback",
        2,
        "inert: commit-boost registers no v2->v1 fallback counter, so this check could only ever \
         PASS. Relay v2 support is owned by cb_relay_v2_unsupported",
    )
}

/// Standard Prometheus-style histogram_quantile: find bucket where cumulative
/// count >= q*total, linearly interpolate between lower-le and le.
///
/// `buckets` is (le, cumulative_count) sorted ascending. The last bucket is
/// expected to be `+Inf` (f64::INFINITY) with the grand total count.
pub fn histogram_quantile(q: f64, buckets: &[(f64, f64)]) -> Option<f64> {
    if buckets.is_empty() || !(0.0..=1.0).contains(&q) {
        return None;
    }
    let total = buckets.last().map(|(_, c)| *c).unwrap_or(0.0);
    if total <= 0.0 {
        return None;
    }
    let rank = q * total;

    // Find first bucket where cumulative count >= rank.
    let mut idx = 0usize;
    while idx < buckets.len() && buckets[idx].1 < rank {
        idx += 1;
    }
    if idx == buckets.len() {
        return None;
    }

    let (le, cum) = buckets[idx];
    if le.is_infinite() {
        for i in (0..idx).rev() {
            if buckets[i].0.is_finite() {
                return Some(buckets[i].0);
            }
        }
        return None;
    }

    let (lower_le, lower_cum) = if idx == 0 {
        (0.0, 0.0)
    } else {
        buckets[idx - 1]
    };
    let lower_le = if lower_le.is_infinite() {
        0.0
    } else {
        lower_le
    };

    let span = cum - lower_cum;
    if span <= 0.0 {
        return Some(le);
    }
    Some(lower_le + (le - lower_le) * ((rank - lower_cum) / span))
}

/// Check p95 get_header latency (histogram) from CB Prometheus metrics.
///
/// Aggregates across all `{endpoint, relay_id}` dimensions of the
/// `cb_pbs_relay_latency` histogram into a single global distribution.
/// We do this because a p95 per relay is rarely useful for our purposes;
/// what matters is "did at least one relay answer fast enough often".
///
/// # Histogram parsing
///
/// `prometheus-parse` collapses histogram families into a single
/// `Value::Histogram(Vec<HistogramCount>)` sample under the bare metric
/// name (NOT `_bucket` suffix). Earlier versions of this code grepped for
/// `cb_pbs_relay_latency_bucket` and always found nothing -- that's why
/// the p95 check used to SKIP even when histogram data was present.
pub fn check_relay_latency(scrape: &Scrape, threshold_ms: f64) -> CheckResult {
    let mut by_le: BTreeMap<String, f64> = BTreeMap::new();
    let mut any_bucket = false;
    for s in &scrape.samples {
        if s.metric != "cb_pbs_relay_latency" {
            continue;
        }
        if let prometheus_parse::Value::Histogram(buckets) = &s.value {
            any_bucket = true;
            for hc in buckets {
                // `hc.less_than` is the le bound; f64::INFINITY for +Inf.
                let key = if hc.less_than.is_infinite() {
                    "+Inf".to_string()
                } else {
                    format!("{}", hc.less_than)
                };
                *by_le.entry(key).or_insert(0.0) += hc.count;
            }
        }
    }

    if !any_bucket {
        return CheckResult::skip(
            "cb_relay_latency",
            2,
            "cb_pbs_relay_latency histogram not exposed",
        );
    }

    let mut buckets: Vec<(f64, f64)> = by_le
        .into_iter()
        .filter_map(|(k, v)| {
            let le = if k == "+Inf" {
                f64::INFINITY
            } else {
                k.parse::<f64>().ok()?
            };
            Some((le, v))
        })
        .collect();
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let total = buckets.last().map(|(_, c)| *c).unwrap_or(0.0);
    if total <= 0.0 {
        return CheckResult::skip(
            "cb_relay_latency",
            2,
            "No relay latency observations yet (count=0)",
        );
    }

    let p95 = match histogram_quantile(0.95, &buckets) {
        Some(v) => v,
        None => {
            return CheckResult::skip(
                "cb_relay_latency",
                2,
                "histogram_quantile returned no value (degenerate buckets)",
            );
        }
    };

    // Histogram le values in the CB registry are in SECONDS (prometheus
    // default). Convert for the threshold check and display.
    let p95_ms = p95 * 1000.0;
    let data = serde_json::json!({
        "p95_ms": p95_ms,
        "threshold_ms": threshold_ms,
        "total_samples": total as u64,
    });

    if p95_ms < threshold_ms {
        CheckResult::pass(
            "cb_relay_latency",
            2,
            format!(
                "p95 relay latency {p95_ms:.1}ms < {threshold_ms}ms threshold ({total:.0} samples)"
            ),
        )
        .with_data(data)
    } else {
        CheckResult::warn(
            "cb_relay_latency",
            2,
            format!(
                "p95 relay latency {p95_ms:.1}ms >= {threshold_ms}ms threshold ({total:.0} samples)"
            ),
        )
        .with_data(data)
    }
}

/// Run all CB metrics checks.
///
/// Tries HTTP fetch first, falls back to kurtosis exec if needed.
///
/// `strict` controls whether soft warnings (zero deliveries, zero bids)
/// are promoted to hard failures. See [`classify_endpoint`].
pub async fn run_metrics_checks(
    http_client: &reqwest::Client,
    metrics_url: Option<&str>,
    enclave: Option<&str>,
    cb_services: &[String],
    strict: bool,
) -> Vec<CheckResult> {
    let skip_all = |reason: &str| -> Vec<CheckResult> {
        [
            "cb_get_header_matrix",
            "cb_register_validator_matrix",
            "cb_submit_blinded_block_matrix",
            "cb_status_matrix",
            "cb_v2_fallback",
            "cb_relay_latency",
        ]
        .iter()
        .map(|id| CheckResult::skip(*id, 2, reason))
        .collect()
    };

    if let Some(url) = metrics_url {
        match metrics::fetch_metrics(http_client, url).await {
            Ok(scrape) => return run_checks_on_scrape(&scrape, strict),
            Err(e) => {
                tracing::warn!("HTTP metrics fetch failed: {e}, trying exec fallback");
            }
        }
    }

    if let (Some(enclave), Some(service)) = (enclave, cb_services.first()) {
        tracing::info!("Fetching metrics via exec from {service}");
        match metrics::fetch_metrics_via_exec(enclave, service, METRICS_PORT) {
            Ok(scrape) => return run_checks_on_scrape(&scrape, strict),
            Err(e) => {
                tracing::warn!("Exec metrics fetch failed: {e}");
            }
        }
    }

    skip_all(
        "Metrics not available (CB needs metrics config; not set in default kurtosis PBS mode)",
    )
}

fn run_checks_on_scrape(scrape: &Scrape, strict: bool) -> Vec<CheckResult> {
    let endpoints = [
        "get_header",
        "register_validator",
        "submit_blinded_block",
        "status",
    ];
    let mut out: Vec<CheckResult> = endpoints
        .iter()
        .map(|ep| {
            let stats = collect_endpoint_stats(scrape, ep);
            classify_endpoint(ep, &stats, strict)
        })
        .collect();

    out.push(check_v2_fallback(scrape));
    out.push(check_v2_unsupported(scrape));
    out.push(check_relay_latency(scrape, 500.0));

    // Tier-1 escalation: any matrix FAIL (from 5xx) should fail the overall
    // run. The matrix checks are tier 2, but 5xx is a real pipeline failure
    // -- escalate it to tier 1 so report::exit_code sees it. The same applies
    // to v2-unsupported: every builder block the proposer chose was LOST, which
    // is at least as fatal as a 5xx.
    for c in out.iter_mut() {
        if c.status == CheckStatus::Fail
            && (c.id.ends_with("_matrix") || c.id == "cb_relay_v2_unsupported")
        {
            c.tier = 1;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Scrape {
        let lines = text.lines().map(|l| Ok(l.to_owned()));
        Scrape::parse(lines).expect("valid prometheus text")
    }

    #[test]
    fn bucket_code_categories() {
        assert_eq!(bucket_code("200"), "200");
        assert_eq!(bucket_code("202"), "202");
        assert_eq!(bucket_code("204"), "204");
        assert_eq!(bucket_code("400"), "4xx");
        assert_eq!(bucket_code("404"), "4xx");
        assert_eq!(bucket_code("500"), "5xx");
        assert_eq!(bucket_code("502"), "5xx");
        assert_eq!(bucket_code("201"), "other");
        assert_eq!(bucket_code("garbage"), "other");
        // CB's synthetic client-timeout marker MUST NOT bucket as relay 5xx —
        // that misattribution tier-1-failed a run whose relays served 0 errors.
        assert_eq!(bucket_code("555"), "timeout");
        // CB's synthetic WS transport-error marker (PR-483 streaming): also
        // client-observed, same misattribution class as 555.
        assert_eq!(bucket_code("556"), "transport");
        // A real (unusual) HTTP 5xx that is neither synthetic code stays 5xx.
        assert_eq!(bucket_code("550"), "5xx");
        assert_eq!(bucket_code("557"), "5xx");
    }

    // --- timeout (555) classification: the timing-games false-red fix --------

    #[test]
    fn classify_heavy_timeouts_warn_not_fail() {
        // The live-captured timing-games shape (2026-08-03): healthy bids plus
        // ~42% CB-deadline cancellations, ZERO relay-served 5xx. Old bucketing
        // called this "47.5% relay 5xx" -> tier-1 FAIL. Must be WARN.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 27.0);
        s.add_relay("r0", "204", 4.0);
        s.add_relay("r0", "400", 2.0);
        s.add_relay("r0", "555", 17.0);
        s.add_relay("r1", "200", 21.0);
        s.add_relay("r1", "204", 2.0);
        s.add_relay("r1", "400", 2.0);
        s.add_relay("r1", "555", 25.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Warn, "timeouts must not FAIL");
        assert!(
            r.detail.contains("555"),
            "detail names the marker: {}",
            r.detail
        );
        assert!(r.detail.contains("not relay-served"));
    }

    #[test]
    fn classify_heavy_timeouts_warn_even_under_strict() {
        // --strict is zero-tolerance for PIPELINE errors; a 555 is CB's own
        // deadline policy, so strict must not promote it to FAIL.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 10.0);
        s.add_relay("r0", "555", 30.0);
        let r = classify_endpoint("get_header", &s, true);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn classify_few_timeouts_pass_with_note() {
        // Below MAX_TIMEOUT_RATE: normal classification, timeout count noted.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 90.0);
        s.add_relay("r0", "555", 5.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("timeout"), "detail: {}", r.detail);
    }

    #[test]
    fn classify_heavy_transport_errors_warn_not_fail() {
        // A stream relay refusing connections every slot: heavy 556, zero real
        // 5xx. Must WARN (relay death is the tier-1 relay checks' call), never
        // FAIL, not even under --strict.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 10.0);
        s.add_relay("r0", "556", 30.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Warn, "556 must not FAIL");
        assert!(r.detail.contains("556"), "detail names 556: {}", r.detail);
        let strict = classify_endpoint("get_header", &s, true);
        assert_eq!(strict.status, CheckStatus::Warn, "strict must not promote");
    }

    #[test]
    fn mixed_555_and_556_aggregate_into_one_warn() {
        // Timing-games over a flaky stream: both synthetic codes present. One
        // WARN naming both counts, not two competing verdicts.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 10.0);
        s.add_relay("r0", "555", 10.0);
        s.add_relay("r0", "556", 10.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("555") && r.detail.contains("556"));
    }

    #[test]
    fn few_transport_errors_pass_with_note() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 90.0);
        s.add_relay("r0", "556", 5.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("556"), "detail: {}", r.detail);
    }

    #[test]
    fn real_5xx_still_fails_despite_heavy_transport_errors() {
        // 556 excluded from the 5xx denominator, same as 555: a genuine relay
        // error storm FAILs even amid heavy stream-transport failures.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 4.0);
        s.add_relay("r0", "500", 10.0);
        s.add_relay("r0", "556", 90.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Fail, "real 5xx must keep failing");
    }

    #[test]
    fn real_5xx_still_fails_despite_heavy_timeouts() {
        // Timeouts are excluded from the 5xx denominator, so a genuine relay
        // error storm FAILs even when timeout polling dominates raw counts:
        // 10 x 500 vs 14 completed responses = 71% > 25%, regardless of 90 x 555.
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 4.0);
        s.add_relay("r0", "500", 10.0);
        s.add_relay("r0", "555", 90.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Fail, "real 5xx must keep failing");
        assert!(r.detail.contains("5xx"));
    }

    #[test]
    fn collect_get_header_stats_relay_and_beacon() {
        let text = r#"# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r0"} 14
cb_pbs_relay_status_code_total{http_status_code="204",endpoint="get_header",relay_id="r0"} 18
cb_pbs_relay_status_code_total{http_status_code="500",endpoint="get_header",relay_id="r1"} 2
# HELP cb_pbs_beacon_node_status_code_total beacon codes
# TYPE cb_pbs_beacon_node_status_code_total counter
cb_pbs_beacon_node_status_code_total{http_status_code="200",endpoint="get_header"} 14
cb_pbs_beacon_node_status_code_total{http_status_code="204",endpoint="get_header"} 18
"#;
        let scrape = parse(text);
        let s = collect_endpoint_stats(&scrape, "get_header");
        // relay side
        assert_eq!(s.relay_get("200"), 14.0);
        assert_eq!(s.relay_get("204"), 18.0);
        assert_eq!(s.relay_get("5xx"), 2.0);
        assert_eq!(
            s.relay_by_id.get("r0").unwrap().get("200").copied(),
            Some(14.0)
        );
        assert_eq!(
            s.relay_by_id.get("r1").unwrap().get("5xx").copied(),
            Some(2.0)
        );
        // beacon side
        assert_eq!(s.beacon_get("200"), 14.0);
        assert_eq!(s.beacon_get("204"), 18.0);
    }

    #[test]
    fn classify_get_header_pass_with_bids() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 14.0);
        s.add_relay("r0", "204", 18.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn classify_get_header_warn_only_204() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "204", 32.0);
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("--strict"));
    }

    #[test]
    fn classify_get_header_strict_only_204_fails() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "204", 32.0);
        let r = classify_endpoint("get_header", &s, true);
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn classify_5xx_transient_warns_but_strict_and_high_rate_fail() {
        // H2: a few 5xx against many good requests (1/11 = 9% < 25%) is warmup
        // noise → WARN (not the old FAIL), but --strict still FAILs.
        let mut low = EndpointStats::default();
        low.add_relay("r0", "200", 10.0);
        low.add_relay("r0", "500", 1.0);
        assert_eq!(
            classify_endpoint("get_header", &low, false).status,
            CheckStatus::Warn
        );
        assert_eq!(
            classify_endpoint("get_header", &low, true).status,
            CheckStatus::Fail
        );

        // A materially broken relay (5/6 = 83% > 25%) FAILs even without --strict.
        let mut high = EndpointStats::default();
        high.add_relay("r0", "200", 1.0);
        high.add_relay("r0", "500", 5.0);
        assert_eq!(
            classify_endpoint("get_header", &high, false).status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn classify_submit_blinded_block_v1_pass() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 3.0);
        let r = classify_endpoint("submit_blinded_block", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("3 v1"));
    }

    #[test]
    fn classify_submit_blinded_block_v2_pass() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "202", 4.0);
        let r = classify_endpoint("submit_blinded_block", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("4 v2"));
    }

    #[test]
    fn classify_submit_blinded_block_mixed_pass() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 2.0);
        s.add_relay("r0", "202", 3.0);
        let r = classify_endpoint("submit_blinded_block", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("5 total"));
    }

    #[test]
    fn classify_submit_blinded_block_no_submissions_warn() {
        // Genuinely zero submissions: the proposer never chose a builder block.
        // (204s are the "no bid" shape; no 4xx = nothing was ever submitted.)
        let mut s = EndpointStats::default();
        s.add_relay("r0", "204", 1.0);
        let r = classify_endpoint("submit_blinded_block", &s, false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("never chose"), "detail: {}", r.detail);
        assert!(r.detail.contains("--strict"));
    }

    #[test]
    fn classify_submit_blinded_block_no_submissions_strict_fails() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "204", 1.0);
        let r = classify_endpoint("submit_blinded_block", &s, true);
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn classify_submit_blinded_block_relay_rejected_is_not_never_chose() {
        // The live nethermind+prysm shape: 26 blinded blocks submitted, ALL
        // rejected 4xx by the relay. The old code called this "proposer never
        // chose a builder block" — a wrong diagnosis pointing at the wrong
        // component. Must FAIL and name the relay as the rejecter.
        let mut s = EndpointStats::default();
        s.add_relay("mev_relay_0", "400", 26.0);
        let r = classify_endpoint("submit_blinded_block", &s, false);
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "relay refusing every block is not a WARN"
        );
        assert!(r.detail.contains("REJECTED"), "detail: {}", r.detail);
        assert!(
            !r.detail.contains("never chose"),
            "must NOT misdiagnose as proposer-side: {}",
            r.detail
        );
    }

    #[test]
    fn classify_register_validator_all_accepted_pass() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 26.0);
        s.add_beacon("200", 26.0);
        let r = classify_endpoint("register_validator", &s, false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains("100%"));
    }

    #[test]
    fn classify_register_validator_partial_warn_shows_both_sides() {
        // Mirrors the live devnet run: relay rejected 19 batches, CB
        // translated each to 502 toward CL. Both views should appear, and
        // the check should WARN (not FAIL, not double-count).
        let mut s = EndpointStats::default();
        s.add_relay("r0", "200", 26.0);
        s.add_relay("r0", "400", 19.0);
        s.add_beacon("200", 26.0);
        s.add_beacon("502", 19.0);

        let r = classify_endpoint("register_validator", &s, false);
        assert_eq!(r.status, CheckStatus::Warn);
        // Acceptance rate shown (26/45 ~= 57.8%, rounds to 58)
        assert!(r.detail.contains("58%"), "detail: {}", r.detail);
        // Beacon-side 502 translation surfaced
        assert!(r.detail.contains("502 to CL"), "detail: {}", r.detail);
        // data JSON has both sides separated
        let d = &r.data;
        assert_eq!(d["relay_side"]["totals"]["200"], 26);
        assert_eq!(d["relay_side"]["totals"]["4xx"], 19);
        assert_eq!(d["beacon_side"]["totals"]["200"], 26);
        assert_eq!(d["beacon_side"]["totals"]["5xx"], 19);
    }

    #[test]
    fn classify_register_validator_only_4xx_fails() {
        let mut s = EndpointStats::default();
        s.add_relay("r0", "400", 4.0);
        assert_eq!(
            classify_endpoint("register_validator", &s, false).status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn classify_empty_stats_skip() {
        let s = EndpointStats::default();
        let r = classify_endpoint("get_header", &s, false);
        assert_eq!(r.status, CheckStatus::Skip);
    }

    #[test]
    fn v2_fallback_is_inert_and_says_so() {
        // The metric it read does not exist in commit-boost, so the old logic
        // could only ever return PASS. An always-green check is worse than no
        // check: it also asserted "relays support v2" on a run where the relay
        // was 404ing every v2 submission.
        let r = check_v2_fallback(&parse(""));
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(r.detail.contains("inert"), "detail: {}", r.detail);
        assert!(
            r.detail.contains("cb_relay_v2_unsupported"),
            "must point at the check that actually owns v2 support"
        );
    }

    #[test]
    fn v2_unsupported_nonzero_fails_and_names_the_relay() {
        // The live nethermind+prysm shape: prysm submits via v2, helix 404s the
        // v2 route, CB refuses to downgrade -> every builder block is lost.
        let text = r#"# HELP cb_pbs_pbs_submit_block_v2_unsupported_total x
# TYPE cb_pbs_pbs_submit_block_v2_unsupported_total counter
cb_pbs_pbs_submit_block_v2_unsupported_total{relay_id="mev_relay_0"} 11
"#;
        let scrape = parse(text);
        let r = check_v2_unsupported(&scrape);
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "lost submissions are not a WARN"
        );
        assert!(
            r.detail.contains("mev_relay_0"),
            "names the relay: {}",
            r.detail
        );
        assert_eq!(r.data["total"], 11);
    }

    #[test]
    fn v2_unsupported_missing_counter_passes() {
        // Prometheus omits never-incremented families; absence == zero == fine.
        assert_eq!(check_v2_unsupported(&parse("")).status, CheckStatus::Pass);
    }

    #[test]
    fn v2_unsupported_escalates_to_tier1_on_fail() {
        // A relay that cannot serve the proposer's submissions must gate the
        // exit code, like a matrix 5xx.
        let text = r#"# HELP cb_pbs_pbs_submit_block_v2_unsupported_total x
# TYPE cb_pbs_pbs_submit_block_v2_unsupported_total counter
cb_pbs_pbs_submit_block_v2_unsupported_total{relay_id="mev_relay_0"} 3
"#;
        let mut c = check_v2_unsupported(&parse(text));
        assert_eq!(c.tier, 2, "authored at tier 2");
        // Mirror run_metrics_checks' escalation rule.
        if c.status == CheckStatus::Fail
            && (c.id.ends_with("_matrix") || c.id == "cb_relay_v2_unsupported")
        {
            c.tier = 1;
        }
        assert_eq!(c.tier, 1, "must escalate so the run fails");
    }

    #[test]
    fn relay_latency_histogram_parses_and_computes_p95() {
        // Real CB output shape: histogram family with le=... buckets.
        // 345 samples total, p95 ~= 2.5s bucket's le.
        let text = r#"# HELP cb_pbs_relay_latency HTTP latency by relay
# TYPE cb_pbs_relay_latency histogram
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.005"} 0
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.01"} 7
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.025"} 109
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.05"} 215
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.1"} 249
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.25"} 267
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="0.5"} 277
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="1"} 306
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="2.5"} 345
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="5"} 345
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="10"} 345
cb_pbs_relay_latency_bucket{endpoint="submit_blinded_block",relay_id="r0",le="+Inf"} 345
cb_pbs_relay_latency_sum{endpoint="submit_blinded_block",relay_id="r0"} 95.32
cb_pbs_relay_latency_count{endpoint="submit_blinded_block",relay_id="r0"} 345
"#;
        let scrape = parse(text);
        // Very generous threshold so we assert PASS from real-ish data.
        let r = check_relay_latency(&scrape, 5000.0);
        // Should not skip -- histogram was exposed and parsed.
        assert_ne!(
            r.status,
            CheckStatus::Skip,
            "histogram should parse: {}",
            r.detail
        );
        // p95 should be in ms range (seconds * 1000). For this data,
        // 0.95 * 345 = 327.75 which is in the le=1 bucket (306..345),
        // interpolating to roughly 1000ms.
        let p95_ms = r.data["p95_ms"].as_f64().expect("p95_ms");
        assert!((100.0..=2500.0).contains(&p95_ms), "p95_ms={p95_ms}");
    }
}
