//! Aggregated-bidding verification: did Commit-Boost deliver at least the best
//! bid it was OFFERED across relays?
//!
//! Data source (the point of this module vs the reverted first attempt): the
//! per-relay bids come from CB's own "received new header" getHeader log events
//! (`relay_id` + `slot` + `value_eth`) — i.e. exactly what CB compared when it
//! picked a winner — NOT the relay data API's `builder_blocks_received`, which
//! also includes builder submissions that failed simulation and were never
//! offered to the proposer (that source over-stated the bid and false-alarmed).
//! These log events are full-coverage (every getHeader is logged), so there is no
//! slot sampling. The delivered value comes from the relay data API (the actually
//! delivered payload). Both `value_eth` (parsed decimal->wei, exactly, no float)
//! and the delivered U256 are exact wei of the same quantity, so they compare
//! directly — a correct selection delivers the winning bid's exact value.

use std::collections::BTreeMap;

use alloy_primitives::U256;

use crate::checks::CheckResult;
use crate::checks::mux_routing::{fetch_service_logs, parse_cb_log_line};
use crate::relay::RelayClient;

/// Parse a `value_eth` decimal string (e.g. "0.050439063999832000") to integer
/// wei, WITHOUT floating point (exact). Returns None on a malformed value.
pub fn value_eth_to_wei(s: &str) -> Option<u128> {
    let s = s.trim().trim_matches('"');
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // 18 decimal places = wei. Pad/truncate the fractional part to 18 digits.
    let mut frac18 = String::with_capacity(18);
    frac18.push_str(frac);
    frac18.truncate(18);
    while frac18.len() < 18 {
        frac18.push('0');
    }
    let combined = format!("{whole}{frac18}");
    // Genuine zero and (physically impossible) u128 overflow both collapse to 0,
    // which understates a bid — the safe direction (never a false shortfall).
    combined
        .trim_start_matches('0')
        .parse::<u128>()
        .ok()
        .or(Some(0))
}

fn u256_wei_to_u128(v: U256) -> u128 {
    v.try_into().unwrap_or(u128::MAX)
}

/// Verify aggregated bidding for a multi-relay run. SKIPs single-relay runs (no
/// competition possible). Fetches per-relay offered bids from CB logs + delivered
/// values from the relays, then classifies.
pub async fn check_best_bid_selection(
    enclave: &str,
    cb_service_names: &[String],
    relays: &[RelayClient],
    start_slot: u64,
    end_slot: u64,
) -> CheckResult {
    if relays.len() < 2 {
        return CheckResult::skip(
            "relay.best_bid",
            2,
            "Single-relay scenario — no cross-relay aggregation to verify",
        );
    }

    // Per-relay offered bids (wei) from CB "received new header" getHeader logs,
    // restricted to the observation window so they line up with delivered data.
    let mut bids_by_slot: BTreeMap<u64, Vec<(String, u128)>> = BTreeMap::new();
    for service in cb_service_names {
        let logs = match fetch_service_logs(enclave, service) {
            Ok(l) => l,
            Err(_) => continue,
        };
        for line in logs.lines() {
            let Some(ev) = parse_cb_log_line(line) else {
                continue;
            };
            if !ev.message.starts_with("received new header") {
                continue;
            }
            let (Some(slot), Some(relay_id)) = (ev.slot, ev.relay_id.as_ref()) else {
                continue;
            };
            if slot < start_slot || slot > end_slot {
                continue;
            }
            let Some(wei) = ev.fields.get("value_eth").and_then(|v| value_eth_to_wei(v)) else {
                continue;
            };
            bids_by_slot
                .entry(slot)
                .or_default()
                .push((relay_id.clone(), wei));
        }
    }

    // Delivered (winning) value per slot (wei) from the relay data API.
    let mut delivered_by_slot: BTreeMap<u64, u128> = BTreeMap::new();
    for relay in relays {
        if let Ok(payloads) = relay.get_payloads_delivered(start_slot, end_slot).await {
            for p in payloads {
                let w = u256_wei_to_u128(p.value);
                let e = delivered_by_slot.entry(p.slot).or_insert(w);
                if w > *e {
                    *e = w;
                }
            }
        }
    }

    classify_best_bid(&bids_by_slot, &delivered_by_slot)
}

/// Pure verdict logic (Law 4 seam; generic over the value type so tests use a
/// trivial `V`). Contract:
/// - NO slot had >=2 relays offering bids → WARN: aggregation was never exercised.
/// - competitive slots exist but NONE had a delivered payload to compare against →
///   WARN: nothing was actually verified (a competitive slot only counts as
///   verified when we have a delivered value for it — otherwise we'd green having
///   compared nothing, the Law 3 false-green).
/// - a VERIFIED competitive slot where delivered < the best OFFERED bid →
///   suboptimal selection → recorded, verdict WARN.
/// - otherwise → PASS, over the verified competitive slots.
pub fn classify_best_bid<V>(
    bids_by_slot: &BTreeMap<u64, Vec<(String, V)>>,
    delivered_by_slot: &BTreeMap<u64, V>,
) -> CheckResult
where
    V: Copy + Ord + std::fmt::Display,
{
    let competitive: Vec<(u64, &Vec<(String, V)>)> = bids_by_slot
        .iter()
        .filter(|(_, bids)| {
            bids.iter()
                .map(|(r, _)| r)
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= 2
        })
        .map(|(s, b)| (*s, b))
        .collect();

    if competitive.is_empty() {
        return CheckResult::warn(
            "relay.best_bid",
            2,
            format!(
                "No multi-relay bid competition observed across {} slot(s) with bids — aggregated \
                 bidding was NOT exercised (relays did not offer bids on overlapping slots). Not \
                 asserting best-bid selection.",
                bids_by_slot.len()
            ),
        )
        .with_data(
            serde_json::json!({ "competitive_slots": 0, "slots_with_bids": bids_by_slot.len() }),
        );
    }

    // Only a competitive slot WITH a delivered value can actually be verified.
    let mut verified = 0usize;
    let mut suboptimal = Vec::new();
    for (slot, bids) in &competitive {
        let Some(delivered) = delivered_by_slot.get(slot) else {
            continue;
        };
        verified += 1;
        let best = bids.iter().map(|(_, v)| *v).max().unwrap(); // competitive => non-empty
        if *delivered < best {
            suboptimal.push(serde_json::json!({
                "slot": slot,
                "best_offered_bid": best.to_string(),
                "delivered": delivered.to_string(),
            }));
        }
    }

    let n = competitive.len();
    let data = serde_json::json!({
        "competitive_slots": n,
        "verified_slots": verified,
        "unverified_slots": n - verified,
        "suboptimal_count": suboptimal.len(),
        "suboptimal": suboptimal,
    });

    if verified == 0 {
        CheckResult::warn(
            "relay.best_bid",
            2,
            format!(
                "{n} competitive slot(s) had multi-relay bids but NONE had a delivered payload to \
                 compare against (out of window, or the slot was missed) — best-bid selection was \
                 not actually verified."
            ),
        )
        .with_data(data)
    } else if suboptimal.is_empty() {
        CheckResult::pass(
            "relay.best_bid",
            2,
            format!(
                "Aggregated bidding verified across {verified} competitive slot(s) with delivered \
                 payloads: CB delivered >= the best offered per-relay bid."
            ),
        )
        .with_data(data)
    } else {
        CheckResult::warn(
            "relay.best_bid",
            2,
            format!(
                "{} of {verified} verified competitive slot(s) delivered LESS than the best offered \
                 bid (may be a late, rejected, or ineligible header — value left on the table).",
                suboptimal.len()
            ),
        )
        .with_data(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckStatus;

    #[test]
    fn value_eth_parses_to_wei_exactly() {
        assert_eq!(value_eth_to_wei("1"), Some(1_000_000_000_000_000_000));
        assert_eq!(value_eth_to_wei("0.000000001"), Some(1_000_000_000)); // 1 gwei
        assert_eq!(
            value_eth_to_wei("\"0.050439063999832000\""),
            Some(50_439_063_999_832_000)
        );
        assert_eq!(value_eth_to_wei("0"), Some(0));
        assert_eq!(value_eth_to_wei("abc"), None);
    }

    fn bids(pairs: &[(u64, &[(&str, u128)])]) -> BTreeMap<u64, Vec<(String, u128)>> {
        pairs
            .iter()
            .map(|(slot, rs)| (*slot, rs.iter().map(|(r, v)| (r.to_string(), *v)).collect()))
            .collect()
    }
    fn delivered(pairs: &[(u64, u128)]) -> BTreeMap<u64, u128> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn best_bid_warn_when_no_multi_relay_competition() {
        let b = bids(&[(5, &[("relay-a", 100)]), (6, &[("relay-a", 200)])]);
        let d = delivered(&[(5, 100), (6, 200)]);
        let r = classify_best_bid(&b, &d);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.data["competitive_slots"], 0);
    }

    #[test]
    fn best_bid_pass_when_delivered_matches_best() {
        let b = bids(&[(5, &[("relay-a", 100), ("relay-b", 150)])]);
        let d = delivered(&[(5, 150)]);
        let r = classify_best_bid(&b, &d);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["verified_slots"], 1);
        assert_eq!(r.data["suboptimal_count"], 0);
    }

    #[test]
    fn best_bid_warn_when_delivered_below_best() {
        let b = bids(&[(5, &[("relay-a", 100), ("relay-b", 150)])]);
        let d = delivered(&[(5, 100)]);
        let r = classify_best_bid(&b, &d);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.data["suboptimal_count"], 1);
    }

    #[test]
    fn best_bid_warn_when_competitive_but_nothing_delivered_to_compare() {
        // The Law 3 guard: a competitive slot with NO delivered payload (out of
        // window / missed) must NOT count as verified → WARN, never PASS.
        let b = bids(&[(5, &[("relay-a", 100), ("relay-b", 150)])]);
        let d = delivered(&[(9999, 100)]); // different slot; slot 5 has no delivery
        let r = classify_best_bid(&b, &d);
        assert_eq!(
            r.status,
            CheckStatus::Warn,
            "must not PASS having verified nothing"
        );
        assert_eq!(r.data["verified_slots"], 0);
        assert_eq!(r.data["unverified_slots"], 1);
    }
}
