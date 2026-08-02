//! Relay pipeline verification checks for MEV pipeline stages.

use std::collections::HashMap;

use futures::StreamExt;

use crate::beacon::BeaconClient;
use crate::checks::{CheckResult, CheckStatus};
use crate::relay::RelayClient;

/// Check that builder blocks were received by the relay in the given slot range.
///
/// The relay data API requires at least one filter param (slot, block_hash,
/// block_number, or builder_pubkey). We sample slots across the range.
pub async fn check_builder_blocks_received(
    relay: &RelayClient,
    start_slot: u64,
    end_slot: u64,
) -> CheckResult {
    let mut all_entries = Vec::new();
    let range = end_slot.saturating_sub(start_slot).max(1);
    let step = (range / 10).max(1);

    let sample_slots: Vec<u64> = (start_slot..=end_slot).step_by(step as usize).collect();

    for slot in &sample_slots {
        match relay.get_builder_blocks_received(*slot).await {
            Ok(entries) => all_entries.extend(entries),
            Err(_) => continue,
        }
    }

    let count = all_entries.len();
    let data = serde_json::json!({ "count": count });

    if count > 0 {
        CheckResult::pass(
            "relay.builder_blocks_received",
            2,
            format!("Received {count} builder block(s) in slot range [{start_slot}, {end_slot}]"),
        )
        .with_data(data)
    } else {
        CheckResult::fail(
            "relay.builder_blocks_received",
            2,
            format!("No builder blocks received in slot range [{start_slot}, {end_slot}]"),
        )
        .with_data(data)
    }
}

/// Check payloads delivered across multiple relays (union by slot).
pub async fn check_payloads_delivered_multi(
    relays: &[RelayClient],
    start_slot: u64,
    end_slot: u64,
) -> CheckResult {
    let mut by_slot: HashMap<u64, serde_json::Value> = HashMap::new();

    for relay in relays {
        match relay.get_payloads_delivered(start_slot, end_slot).await {
            Ok(payloads) => {
                for p in payloads {
                    by_slot.entry(p.slot).or_insert_with(|| {
                        serde_json::json!({
                            "slot": p.slot,
                            "block_hash": format!("{:#x}", p.block_hash),
                            "value": p.value.to_string(),
                        })
                    });
                }
            }
            Err(e) => {
                tracing::warn!("Failed to get payloads from {}: {e}", relay.base_url());
            }
        }
    }

    let count = by_slot.len();
    let payloads: Vec<_> = by_slot.into_values().collect();

    if count > 0 {
        CheckResult::pass(
            "relay.payloads_delivered_multi",
            1,
            format!(
                "Delivered {count} payload(s) across {} relay(s) in [{start_slot}, {end_slot}]",
                relays.len()
            ),
        )
        .with_data(serde_json::json!({ "count": count, "payloads": payloads }))
    } else {
        CheckResult::fail(
            "relay.payloads_delivered_multi",
            1,
            format!(
                "No payloads delivered across {} relay(s) in [{start_slot}, {end_slot}]",
                relays.len()
            ),
        )
        .with_data(serde_json::json!({ "count": 0 }))
    }
}

/// Check MEV delivery rate: relay payloads vs on-chain blocks.
pub async fn check_mev_delivery_rate(
    relays: &[RelayClient],
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
    threshold: f64,
) -> CheckResult {
    // UNION delivered payloads across ALL relays (not just the first that answers).
    // Under mux each relay holds only the half of deliveries it won, so taking the
    // first relay undercounts the MEV rate and spuriously WARNs (H3). We only SKIP
    // if NO relay answered the data API at all.
    let mut delivered = Vec::new();
    let mut any_ok = false;
    let mut last_error = None;
    for relay in relays {
        match relay.get_payloads_delivered(start_slot, end_slot).await {
            Ok(p) => {
                delivered.extend(p);
                any_ok = true;
            }
            Err(e) => {
                tracing::warn!(
                    "Relay {} doesn't support the data API ({e})",
                    relay.base_url()
                );
                last_error = Some(e);
            }
        }
    }
    if !any_ok {
        let err = last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no relays".to_string());
        return CheckResult::skip(
            "relay.mev_delivery_rate",
            2,
            format!("No relay supports the data API. Last error: {err}"),
        );
    }

    let delivered_hashes: std::collections::HashSet<_> =
        delivered.iter().map(|p| p.block_hash).collect();

    let mut mev_blocks = 0u64;
    let mut total_blocks = 0u64;
    let mut missed = 0u64;

    // Gather the per-slot block hashes concurrently (bounded). The counting fold
    // below is identical to the old serial loop — order-independent counters, so
    // buffer_unordered's out-of-order completion does not change the result.
    let fetched: Vec<_> = futures::stream::iter(start_slot..=end_slot)
        .map(|slot| async move { (slot, beacon.get_block_hash(slot).await) })
        .buffer_unordered(16)
        .collect()
        .await;

    for (_slot, res) in fetched {
        match res {
            Ok(None) => missed += 1,
            Err(_) => missed += 1,
            Ok(Some(hash)) => {
                total_blocks += 1;
                if delivered_hashes.contains(&hash) {
                    mev_blocks += 1;
                }
            }
        }
    }

    // Delegate the verdict to the pure classifier, then attach the missed-slot
    // count (context the rate logic doesn't need to reach a verdict, but the
    // report records — mirrors chain_health::check_missed_slots).
    let mut result = classify_mev_rate(mev_blocks, total_blocks, threshold);
    if let Some(obj) = result.data.as_object_mut() {
        obj.insert("missed_slots".to_string(), serde_json::json!(missed));
    }
    result
}

/// Classify the MEV delivery rate verdict from the delivered/proposed counts.
///
/// Pure decision core extracted from [`check_mev_delivery_rate`] (the P3 pattern
/// — see `chain_health::classify_missed_slots`):
/// - `total_blocks == 0` => FAIL (no proposed blocks to measure against)
/// - `rate >= threshold` => PASS
/// - otherwise           => WARN
///
/// `rate = mev_blocks / total_blocks` (0.0 when there are no proposed blocks).
/// The caller ([`check_mev_delivery_rate`]) attaches the `missed_slots` count to
/// the returned `data` payload afterward.
pub fn classify_mev_rate(mev_blocks: u64, total_blocks: u64, threshold: f64) -> CheckResult {
    let rate = if total_blocks > 0 {
        mev_blocks as f64 / total_blocks as f64
    } else {
        0.0
    };

    let data = serde_json::json!({
        "mev_blocks": mev_blocks,
        "total_blocks": total_blocks,
        "rate": (rate * 10000.0).round() / 10000.0,
    });

    if total_blocks == 0 {
        CheckResult::fail("relay.mev_delivery_rate", 2, "No proposed blocks found").with_data(data)
    } else if rate >= threshold {
        CheckResult::pass(
            "relay.mev_delivery_rate",
            2,
            format!(
                "MEV delivery rate {:.2}% >= {:.2}% threshold",
                rate * 100.0,
                threshold * 100.0
            ),
        )
        .with_data(data)
    } else {
        CheckResult::warn(
            "relay.mev_delivery_rate",
            2,
            format!(
                "MEV delivery rate {:.2}% below {:.2}% threshold",
                rate * 100.0,
                threshold * 100.0
            ),
        )
        .with_data(data)
    }
}

/// Check validator registrations with the relay (tier 3).
///
/// For each pubkey, queries the relay's validator_registration endpoint. PASS
/// if all registered, WARN if some missing, FAIL if none registered. The caller
/// should SKIP outright if the pubkey list is empty (we also handle that
/// defensively).
pub async fn check_validator_registrations(
    relay_url: &str,
    client: &reqwest::Client,
    pubkeys: &[String],
) -> CheckResult {
    if pubkeys.is_empty() {
        return CheckResult::skip(
            "relay.validator_registrations",
            3,
            "No validator pubkeys provided",
        );
    }

    let mut registered: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let base = relay_url.trim_end_matches('/');
    for pk in pubkeys {
        let ok = match client
            .get(format!("{base}/relay/v1/data/validator_registration"))
            .query(&[("pubkey", pk.as_str())])
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                tracing::warn!("Failed to check registration for {pk}: {e}");
                false
            }
        };
        if ok {
            registered.push(pk.clone());
        } else {
            missing.push(pk.clone());
        }
    }

    let total = pubkeys.len();
    let reg_count = registered.len();
    let data = serde_json::json!({
        "registered": reg_count,
        "total": total,
        "missing": missing,
    });

    // The verdict + detail come from the pure classifier; the caller owns the
    // `data` payload (it holds the missing-pubkey vector the counts can't carry).
    classify_registrations(reg_count, missing.len()).with_data(data)
}

/// Classify the validator-registration verdict from the registered/missing counts.
///
/// Pure decision core extracted from [`check_validator_registrations`] (the P3
/// pattern). `total = registered + missing`:
/// - all registered (`missing == 0`) => PASS
/// - some registered, some missing    => WARN
/// - none registered                  => FAIL
///
/// Returns the verdict with an empty `data` payload; the caller attaches the
/// full payload (which includes the list of missing pubkeys) via `with_data`.
pub fn classify_registrations(registered: usize, missing: usize) -> CheckResult {
    let total = registered + missing;

    if registered == total {
        CheckResult::pass(
            "relay.validator_registrations",
            3,
            format!("All {total} validator(s) registered on relay"),
        )
    } else if registered > 0 {
        CheckResult::warn(
            "relay.validator_registrations",
            3,
            format!("{registered}/{total} validator(s) registered; {missing} missing"),
        )
    } else {
        CheckResult::fail(
            "relay.validator_registrations",
            3,
            format!("No validators registered on relay (0/{total})"),
        )
    }
}

/// Verdicts when EVERY relay in the enclave is unreachable at check time. This is
/// NOT benign: the relays were launched and the chain observed a full epoch, so
/// all-dead means the MEV pipeline died mid-run (e.g. relay OOM). The tier-1
/// delivery check must therefore FAIL, not SKIP — a tier-1 SKIP is treated as
/// PASS by `report::exit_code`, which would green a run whose relays died (the C1
/// false-green). The tier-2/3 checks stay SKIP (the tier-1 FAIL already gates).
fn all_relays_dead_results(
    total: usize,
    dead_urls: &[String],
    has_pubkeys: bool,
) -> Vec<CheckResult> {
    let detail = format!(
        "All {total} relay(s) unreachable at check time ({}) — the MEV pipeline is down: relays died \
         or never served during the observation window",
        dead_urls.join(", ")
    );
    let mut out = vec![
        CheckResult::fail("relay.payloads_delivered_multi", 1, detail.clone()),
        CheckResult::skip("relay.builder_blocks_received", 2, detail.clone()),
        CheckResult::skip("relay.mev_delivery_rate", 2, detail.clone()),
    ];
    if has_pubkeys {
        out.push(CheckResult::skip(
            "relay.validator_registrations",
            3,
            detail,
        ));
    }
    out
}

/// Run all relay pipeline checks.
///
/// Probes each relay before running the check batch. If ALL relays are unreachable
/// at check time (mid-run death the startup preflight couldn't catch), the tier-1
/// delivery check FAILs (see `all_relays_dead_results`); otherwise the batch runs
/// against the live relays.
pub async fn run_relay_checks(
    relays: &[RelayClient],
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
    mev_threshold: f64,
    pubkeys: &[String],
    http_client: &reqwest::Client,
) -> Vec<CheckResult> {
    let mut results = Vec::new();

    // Filter to live relays only
    let mut live_relays: Vec<&RelayClient> = Vec::new();
    let mut dead_urls: Vec<String> = Vec::new();
    for relay in relays {
        match relay.ping().await {
            Ok(()) => live_relays.push(relay),
            Err(e) => {
                tracing::warn!("Relay {} unreachable: {e}", relay.base_url());
                dead_urls.push(relay.base_url().to_string());
            }
        }
    }

    if live_relays.is_empty() && !relays.is_empty() {
        results.extend(all_relays_dead_results(
            relays.len(),
            &dead_urls,
            !pubkeys.is_empty(),
        ));
        return results;
    }

    // Rebuild as owned slice of references for downstream calls.
    let live: Vec<RelayClient> = live_relays
        .iter()
        .map(|r| RelayClient::new(r.base_url()))
        .collect();

    // Check builder blocks received across ALL live relays.
    // Aggregated: PASS if any relay received blocks, FAIL only if ALL relays got nothing.
    {
        let mut bb_results: Vec<CheckResult> = Vec::new();
        for relay in &live {
            bb_results.push(check_builder_blocks_received(relay, start_slot, end_slot).await);
        }
        let any_pass = bb_results.iter().any(|r| r.status == CheckStatus::Pass);
        if any_pass {
            let total: usize = bb_results
                .iter()
                .map(|r| r.data.get("count").and_then(|c| c.as_u64()).unwrap_or(0) as usize)
                .sum();
            let details: Vec<&str> = bb_results.iter().map(|r| r.detail.as_str()).collect();
            results.push(
                CheckResult::pass("relay.builder_blocks_received", 2, details.join("; "))
                    .with_data(serde_json::json!({"count": total})),
            );
        } else {
            // No relay passed: surface the worst result (Fail > Warn > Skip).
            // `CheckStatus: Ord` (Fail > Warn > Pass > Skip) reproduces the old
            // hand-rolled key; no Pass is present in this branch by construction.
            let worst = bb_results.into_iter().max_by_key(|r| r.status).unwrap();
            results.push(worst);
        }
    }

    results.push(check_payloads_delivered_multi(&live, start_slot, end_slot).await);

    // Check MEV delivery rate across ALL live relays.
    // Aggregated: best-of status; reports combined delivery stats.
    {
        let mut mv_results: Vec<CheckResult> = Vec::new();
        // Try all relays for delivery data — some may not support the data API
        mv_results.push(
            check_mev_delivery_rate(&live, beacon, start_slot, end_slot, mev_threshold).await,
        );
        // NOTE: this is a BEST-of aggregation (Pass wins), the OPPOSITE of the
        // worst-status `CheckStatus: Ord`, and it collapses Skip => Fail (a Skip
        // result, e.g. no relay supports the data API, lands in the `else`).
        // It is deliberately NOT unified with `worst_status`/`.max()`: doing so
        // would (a) invert the Pass/Fail preference and (b) turn a Skip verdict
        // into the least-severe status instead of Fail.
        let any_pass = mv_results.iter().any(|r| r.status == CheckStatus::Pass);
        let best_status = if any_pass {
            CheckStatus::Pass
        } else if mv_results.iter().any(|r| r.status == CheckStatus::Warn) {
            CheckStatus::Warn
        } else {
            CheckStatus::Fail
        };
        let total_mev: u64 = mv_results
            .iter()
            .map(|r| {
                r.data
                    .get("mev_blocks")
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0)
            })
            .sum();
        let total_blocks: u64 = mv_results
            .iter()
            .map(|r| {
                r.data
                    .get("total_blocks")
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0)
            })
            .sum();
        let details: Vec<&str> = mv_results.iter().map(|r| r.detail.as_str()).collect();
        let data = serde_json::json!({
            "mev_blocks": total_mev,
            "total_blocks": total_blocks,
            "rate": if total_blocks > 0 {
                (total_mev as f64 / total_blocks as f64 * 10000.0).round() / 10000.0
            } else { 0.0 },
        });
        results.push(
            match best_status {
                CheckStatus::Pass => CheckResult::pass(
                    "relay.mev_delivery_rate",
                    2,
                    format!(
                        "MEV delivery rate across all relays: {}",
                        details.join("; ")
                    ),
                ),
                CheckStatus::Warn => CheckResult::warn(
                    "relay.mev_delivery_rate",
                    2,
                    format!("MEV delivery rate below threshold: {}", details.join("; ")),
                ),
                _ => CheckResult::fail(
                    "relay.mev_delivery_rate",
                    2,
                    format!("No MEV deliveries across any relay: {}", details.join("; ")),
                ),
            }
            .with_data(data),
        );
    }

    // Tier 3: per-relay validator registration, aggregated to the worst status.
    if !pubkeys.is_empty() {
        let mut per_relay: Vec<CheckResult> = Vec::new();
        for relay in &live {
            per_relay
                .push(check_validator_registrations(relay.base_url(), http_client, pubkeys).await);
        }
        // Aggregate: FAIL > WARN > PASS; details comma-joined.
        if !per_relay.is_empty() {
            // `CheckStatus: Ord` (Fail > Warn > Pass > Skip). The per-relay
            // results here are only ever Pass/Warn/Fail (this branch is guarded
            // by `!pubkeys.is_empty()`, so check_validator_registrations never
            // returns Skip), so `.max()` reproduces the old fold exactly. The old
            // fold's Skip+Pass=>Pass and empty=>Pass cases only diverge from
            // `.max()` on all-Skip input, which is unreachable here.
            let worst = per_relay
                .iter()
                .map(|r| r.status)
                .max()
                .unwrap_or(CheckStatus::Pass);
            let combined_detail = per_relay
                .iter()
                .enumerate()
                .map(|(i, r)| format!("[{}] {}", live[i].base_url(), r.detail))
                .collect::<Vec<_>>()
                .join("; ");
            let data = serde_json::json!({
                "per_relay": per_relay
                    .iter()
                    .zip(live.iter())
                    .map(|(r, relay)| serde_json::json!({
                        "relay": relay.base_url(),
                        "status": r.status.to_string(),
                        "detail": r.detail,
                        "data": r.data,
                    }))
                    .collect::<Vec<_>>(),
            });
            let agg = match worst {
                CheckStatus::Pass => {
                    CheckResult::pass("relay.validator_registrations", 3, combined_detail)
                }
                CheckStatus::Warn => {
                    CheckResult::warn("relay.validator_registrations", 3, combined_detail)
                }
                CheckStatus::Fail => {
                    CheckResult::fail("relay.validator_registrations", 3, combined_detail)
                }
                CheckStatus::Skip => {
                    CheckResult::skip("relay.validator_registrations", 3, combined_detail)
                }
            }
            .with_data(data);
            results.push(agg);
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_relays_dead_fails_tier1_not_skip() {
        // C1: relays that die mid-run must FAIL the tier-1 delivery check, not SKIP
        // (a tier-1 SKIP is treated as pass by exit_code → false green).
        let r = all_relays_dead_results(2, &["u1".into(), "u2".into()], true);
        let t1 = r
            .iter()
            .find(|c| c.id == "relay.payloads_delivered_multi")
            .expect("tier-1 delivery check present");
        assert_eq!(t1.tier, 1);
        assert_eq!(t1.status, CheckStatus::Fail);
        // The others stay SKIP (tier-1 FAIL already gates the run).
        assert!(
            r.iter()
                .filter(|c| c.tier != 1)
                .all(|c| c.status == CheckStatus::Skip)
        );
    }

    // --- classify_mev_rate (seam 1) -------------------------------------

    // Contract: no proposed blocks (total == 0) FAILs — there's nothing to
    // measure a delivery rate against, so a 0-rate must not silently PASS/WARN.
    #[test]
    fn mev_rate_zero_total_fails() {
        let r = classify_mev_rate(0, 0, 0.5);
        assert_eq!(r.status, CheckStatus::Fail);
        assert_eq!(r.id, "relay.mev_delivery_rate");
        assert_eq!(r.tier, 2);
        assert_eq!(r.detail, "No proposed blocks found");
        assert_eq!(r.data["mev_blocks"], 0);
        assert_eq!(r.data["total_blocks"], 0);
        assert_eq!(r.data["rate"], 0.0);
    }

    // Contract: a rate exactly AT the threshold PASSes (boundary is `>=`).
    #[test]
    fn mev_rate_at_threshold_passes() {
        // 5/10 = 0.5 == 0.5
        let r = classify_mev_rate(5, 10, 0.5);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.contains(">="));
        assert_eq!(r.data["mev_blocks"], 5);
        assert_eq!(r.data["total_blocks"], 10);
        assert_eq!(r.data["rate"], 0.5);
    }

    // Contract: a rate ABOVE the threshold PASSes.
    #[test]
    fn mev_rate_above_threshold_passes() {
        // 9/10 = 0.9 > 0.5
        let r = classify_mev_rate(9, 10, 0.5);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["rate"], 0.9);
    }

    // Contract: a rate BELOW the threshold WARNs (not FAIL — deliveries exist,
    // just under target).
    #[test]
    fn mev_rate_below_threshold_warns() {
        // 2/10 = 0.2 < 0.5
        let r = classify_mev_rate(2, 10, 0.5);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("below"));
        assert_eq!(r.data["rate"], 0.2);
    }

    // Contract: the classifier's data payload carries no `missed_slots` (the
    // caller, check_mev_delivery_rate, attaches it) but everything else matches.
    #[test]
    fn mev_rate_classifier_omits_missed_slots() {
        let r = classify_mev_rate(1, 4, 0.5);
        assert!(r.data.get("missed_slots").is_none());
        assert_eq!(r.data["rate"], 0.25);
    }

    // --- classify_registrations (seam 2) --------------------------------

    // Contract: every validator registered (missing == 0) PASSes.
    #[test]
    fn registrations_all_registered_passes() {
        let r = classify_registrations(3, 0);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.id, "relay.validator_registrations");
        assert_eq!(r.tier, 3);
        assert!(r.detail.contains("All 3 validator(s) registered"));
    }

    // Contract: some registered, some missing WARNs, and the counts appear.
    #[test]
    fn registrations_some_missing_warns() {
        let r = classify_registrations(2, 1);
        assert_eq!(r.status, CheckStatus::Warn);
        // total = registered + missing = 3
        assert!(r.detail.contains("2/3 validator(s) registered; 1 missing"));
    }

    // Contract: none registered FAILs (0/total), even when total > 0.
    #[test]
    fn registrations_none_registered_fails() {
        let r = classify_registrations(0, 4);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.contains("No validators registered on relay (0/4)"));
    }
}
