//! Cross-references relay delivered payloads with on-chain beacon blocks.

use std::collections::BTreeMap;

use alloy_primitives::B256;
use futures::StreamExt;
use tracing::warn;

use crate::beacon::BeaconClient;
use crate::checks::CheckResult;
use crate::relay::RelayClient;

/// Compare relay delivered payload hashes against on-chain beacon block hashes.
///
/// Collects the block_hash EACH relay reported per slot (NOT deduped/first-wins),
/// fetches the on-chain hash per slot, then classifies. Splitting IO from the
/// verdict lets `classify_payload_matches` be unit-tested (Law 4).
pub async fn check_payload_hash_match(
    relays: &[RelayClient],
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
) -> CheckResult {
    // Per slot, every (relay, block_hash) reported. Keeping all of them (instead
    // of first-wins `or_insert`) is what lets us DETECT cross-relay disagreement.
    let mut by_slot: BTreeMap<u64, Vec<(String, B256)>> = BTreeMap::new();
    for relay in relays {
        match relay.get_payloads_delivered(start_slot, end_slot).await {
            Ok(payloads) => {
                for p in payloads {
                    by_slot
                        .entry(p.slot)
                        .or_default()
                        .push((relay.base_url().to_string(), p.block_hash));
                }
            }
            Err(e) => {
                warn!("Failed to fetch payloads from {}: {e}", relay.base_url());
            }
        }
    }

    if by_slot.is_empty() {
        return CheckResult::skip(
            "payload_hash_match",
            1,
            "No delivered payloads to compare (upstream check owns this signal)",
        );
    }

    // Fetch the on-chain hash for each observed slot (None = missing/error),
    // concurrently but bounded. The result map is keyed by slot, so out-of-order
    // completion cannot change it; the Err -> warn+None mapping is preserved.
    let slots: Vec<u64> = by_slot.keys().copied().collect();
    let fetched: Vec<_> = futures::stream::iter(slots)
        .map(|slot| async move { (slot, beacon.get_block_hash(slot).await) })
        .buffer_unordered(16)
        .collect()
        .await;

    let mut chain: BTreeMap<u64, Option<B256>> = BTreeMap::new();
    for (slot, res) in fetched {
        let h = match res {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to get block for slot {slot}: {e}");
                None
            }
        };
        chain.insert(slot, h);
    }

    classify_payload_matches(&by_slot, &chain)
}

/// Pure verdict logic (Law 4 seam). Generic over the hash type so tests use a
/// trivial `H`. WARNs when a slot has divergent relay hashes (relay equivocation)
/// OR when no relay hash matches the on-chain hash — the first-wins union used to
/// silently drop the cross-relay disagreement and could PASS order-dependently.
pub fn classify_payload_matches<H>(
    by_slot: &BTreeMap<u64, Vec<(String, H)>>,
    chain: &BTreeMap<u64, Option<H>>,
) -> CheckResult
where
    H: Copy + Eq + std::hash::Hash + std::fmt::LowerHex,
{
    if by_slot.is_empty() {
        return CheckResult::skip(
            "payload_hash_match",
            1,
            "No delivered payloads to compare (upstream check owns this signal)",
        );
    }

    let mut matched = 0u64;
    let mut mismatched = 0u64;
    let mut missed = 0u64;
    let mut mismatches = Vec::new();
    let mut conflicts = Vec::new();

    for (&slot, relay_hashes) in by_slot {
        // Cross-relay disagreement: >1 distinct block_hash reported for one slot.
        let distinct: std::collections::HashSet<H> = relay_hashes.iter().map(|(_, h)| *h).collect();
        if distinct.len() > 1 {
            conflicts.push(serde_json::json!({
                "slot": slot,
                "relays": relay_hashes
                    .iter()
                    .map(|(r, h)| serde_json::json!({ "relay": r, "block_hash": format!("{h:#x}") }))
                    .collect::<Vec<_>>(),
            }));
            warn!(
                "Payload hash conflict at slot {slot}: {} distinct block hashes reported \
                 (relay equivocation or bug)",
                distinct.len()
            );
        }

        match chain.get(&slot) {
            Some(Some(chain_hash)) => {
                if distinct.iter().any(|h| h == chain_hash) {
                    matched += 1;
                } else {
                    mismatched += 1;
                    mismatches.push(serde_json::json!({
                        "slot": slot,
                        "relay_hashes": distinct.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>(),
                        "chain_hash": format!("{chain_hash:#x}"),
                    }));
                    warn!(
                        "Hash mismatch at slot {slot}: no relay hash matched chain {chain_hash:#x} \
                         (possible reorg)"
                    );
                }
            }
            _ => missed += 1,
        }
    }

    let total = by_slot.len();
    let conflict_count = conflicts.len();
    let detail = format!(
        "{matched} matched, {mismatched} mismatched, {conflict_count} cross-relay conflict(s), \
         {missed} missed out of {total} delivered"
    );
    let data = serde_json::json!({
        "matched": matched,
        "mismatched": mismatched,
        "missed": missed,
        "cross_relay_conflicts": conflict_count,
        "conflicts": conflicts,
        "mismatches": mismatches,
    });

    if mismatched > 0 || conflict_count > 0 {
        CheckResult::warn("payload_hash_match", 1, detail).with_data(data)
    } else {
        CheckResult::pass("payload_hash_match", 1, detail).with_data(data)
    }
}

/// Run all payload matching checks.
pub async fn run_payload_checks(
    relays: &[RelayClient],
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
) -> Vec<CheckResult> {
    vec![check_payload_hash_match(relays, beacon, start_slot, end_slot).await]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckStatus;

    // --- classify_payload_matches verdict tests (u64 stands in for B256) ------

    fn by_slot(pairs: &[(u64, &[(&str, u64)])]) -> BTreeMap<u64, Vec<(String, u64)>> {
        pairs
            .iter()
            .map(|(slot, relays)| {
                (
                    *slot,
                    relays.iter().map(|(r, h)| (r.to_string(), *h)).collect(),
                )
            })
            .collect()
    }

    fn chain(pairs: &[(u64, Option<u64>)]) -> BTreeMap<u64, Option<u64>> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn payload_pass_on_clean_single_relay_match() {
        let bs = by_slot(&[(5, &[("relay-a", 0xaa)])]);
        let ch = chain(&[(5, Some(0xaa))]);
        let r = classify_payload_matches(&bs, &ch);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["matched"], 1);
    }

    #[test]
    fn payload_warn_on_cross_relay_conflict() {
        // The false-green: two relays report DIFFERENT hashes for slot 5. The old
        // first-wins union dropped one and could PASS; now it's detected → WARN.
        let bs = by_slot(&[(5, &[("relay-a", 0xaa), ("relay-b", 0xbb)])]);
        let ch = chain(&[(5, Some(0xaa))]); // one relay even matches chain
        let r = classify_payload_matches(&bs, &ch);
        assert_eq!(r.status, CheckStatus::Warn, "conflict must not pass");
        assert_eq!(r.data["cross_relay_conflicts"], 1);
    }

    #[test]
    fn payload_warn_when_no_relay_matches_chain() {
        let bs = by_slot(&[(5, &[("relay-a", 0xaa)])]);
        let ch = chain(&[(5, Some(0xbb))]);
        let r = classify_payload_matches(&bs, &ch);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.data["mismatched"], 1);
    }

    #[test]
    fn payload_missed_does_not_downgrade_verdict() {
        // A delivered slot with no on-chain block is 'missed', informational only.
        let bs = by_slot(&[(5, &[("relay-a", 0xaa)])]);
        let ch = chain(&[(5, None)]);
        let r = classify_payload_matches(&bs, &ch);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["missed"], 1);
    }

    #[test]
    fn payload_empty_skips() {
        let bs: BTreeMap<u64, Vec<(String, u64)>> = BTreeMap::new();
        let ch: BTreeMap<u64, Option<u64>> = BTreeMap::new();
        assert_eq!(classify_payload_matches(&bs, &ch).status, CheckStatus::Skip);
    }

    // Contract: with no relays there are no delivered payloads to cross-check,
    // so the check must SKIP (it explicitly defers this signal to an upstream
    // check) rather than PASS on zero comparisons. With an empty relay slice the
    // collection loop never runs and the beacon is never queried, so this is a
    // pure, network-free assertion of the empty-input contract.
    #[tokio::test]
    async fn no_relays_skips_not_passes() {
        let beacon = BeaconClient::new("http://127.0.0.1:0");
        let relays: [RelayClient; 0] = [];
        let r = check_payload_hash_match(&relays, &beacon, 0, 10).await;
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(r.id, "payload_hash_match");
        assert_eq!(r.tier, 1);
    }
}
