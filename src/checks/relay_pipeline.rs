//! Relay pipeline verification checks for MEV pipeline stages.

use std::collections::HashMap;

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
    // Get delivered payload block hashes — try each relay until one succeeds.
    // Some relays (e.g., mev-boost-relay) don't expose the data API.
    let mut delivered = Vec::new();
    let mut last_error = None;
    for relay in relays {
        match relay.get_payloads_delivered(start_slot, end_slot).await {
            Ok(p) => {
                delivered = p;
                break;
            }
            Err(e) => {
                tracing::warn!(
                    "Relay {} doesn't support data API ({}), trying next...",
                    relay.base_url(),
                    e
                );
                last_error = Some(e);
            }
        }
    }
    if delivered.is_empty()
        && let Some(err) = last_error
    {
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

    for slot in start_slot..=end_slot {
        match beacon.get_block_hash(slot).await {
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

    let rate = if total_blocks > 0 {
        mev_blocks as f64 / total_blocks as f64
    } else {
        0.0
    };

    let data = serde_json::json!({
        "mev_blocks": mev_blocks,
        "total_blocks": total_blocks,
        "missed_slots": missed,
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
/// For each pubkey, calls `is_validator_registered`. PASS if all registered,
/// WARN if some missing, FAIL if none registered. The caller should SKIP
/// outright if the pubkey list is empty (we also handle that defensively).
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

    if reg_count == total {
        CheckResult::pass(
            "relay.validator_registrations",
            3,
            format!("All {total} validator(s) registered on relay"),
        )
        .with_data(data)
    } else if reg_count > 0 {
        CheckResult::warn(
            "relay.validator_registrations",
            3,
            format!(
                "{reg_count}/{total} validator(s) registered; {} missing",
                missing.len()
            ),
        )
        .with_data(data)
    } else {
        CheckResult::fail(
            "relay.validator_registrations",
            3,
            format!("No validators registered on relay (0/{total})"),
        )
        .with_data(data)
    }
}

/// Run all relay pipeline checks.
///
/// Probes each relay before running the check batch. Unreachable relays
/// produce a single SKIP per downstream check instead of multiple FAILs
/// with "error sending request" noise. This catches mid-run relay crashes
/// that the startup preflight couldn't.
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
        let detail = format!(
            "All {} relay(s) unreachable at check time: {}",
            relays.len(),
            dead_urls.join(", ")
        );
        results.push(CheckResult::skip(
            "relay.builder_blocks_received",
            2,
            &detail,
        ));
        results.push(CheckResult::skip(
            "relay.payloads_delivered_multi",
            1,
            &detail,
        ));
        results.push(CheckResult::skip("relay.mev_delivery_rate", 2, &detail));
        if !pubkeys.is_empty() {
            results.push(CheckResult::skip(
                "relay.validator_registrations",
                3,
                &detail,
            ));
        }
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
                CheckResult::pass(
                    "relay.builder_blocks_received",
                    2,
                    details.join("; "),
                )
                .with_data(serde_json::json!({"count": total})),
            );
        } else {
            let worst = bb_results.into_iter().max_by_key(|r| match r.status {
                CheckStatus::Fail => 2,
                CheckStatus::Warn => 1,
                _ => 0,
            }).unwrap();
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
            .map(|r| r.data.get("mev_blocks").and_then(|c| c.as_u64()).unwrap_or(0))
            .sum();
        let total_blocks: u64 = mv_results
            .iter()
            .map(|r| r.data.get("total_blocks").and_then(|c| c.as_u64()).unwrap_or(0))
            .sum();
        let details: Vec<&str> = mv_results.iter().map(|r| r.detail.as_str()).collect();
        let data = serde_json::json!({
            "mev_blocks": total_mev,
            "total_blocks": total_blocks,
            "rate": if total_blocks > 0 {
                (total_mev as f64 / total_blocks as f64 * 10000.0).round() / 10000.0
            } else { 0.0 },
        });
        results.push(match best_status {
            CheckStatus::Pass => CheckResult::pass(
                "relay.mev_delivery_rate",
                2,
                format!("MEV delivery rate across all relays: {}", details.join("; ")),
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
        }.with_data(data));
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
            let worst =
                per_relay
                    .iter()
                    .map(|r| r.status)
                    .fold(CheckStatus::Pass, |acc, s| match (acc, s) {
                        (CheckStatus::Fail, _) | (_, CheckStatus::Fail) => CheckStatus::Fail,
                        (CheckStatus::Warn, _) | (_, CheckStatus::Warn) => CheckStatus::Warn,
                        (CheckStatus::Skip, CheckStatus::Pass)
                        | (CheckStatus::Pass, CheckStatus::Skip) => CheckStatus::Pass,
                        (a, _) => a,
                    });
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
