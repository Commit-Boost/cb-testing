//! Cross-references relay delivered payloads with on-chain beacon blocks.

use tracing::warn;

use crate::beacon::BeaconClient;
use crate::checks::CheckResult;
use crate::relay::RelayClient;

/// Compare relay delivered payload hashes against on-chain beacon block hashes.
pub async fn check_payload_hash_match(
    relays: &[RelayClient],
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
) -> CheckResult {
    // Collect delivered payloads from all relays, deduped by slot
    let mut by_slot = std::collections::HashMap::new();
    for relay in relays {
        match relay.get_payloads_delivered(start_slot, end_slot).await {
            Ok(payloads) => {
                for p in payloads {
                    by_slot.entry(p.slot).or_insert(p.block_hash);
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

    let mut matched = 0u64;
    let mut mismatched = 0u64;
    let mut missed = 0u64;
    let mut mismatches = Vec::new();

    for (&slot, relay_hash) in &by_slot {
        match beacon.get_block_hash(slot).await {
            Ok(None) => {
                missed += 1;
            }
            Err(e) => {
                warn!("Failed to get block for slot {slot}: {e}");
                missed += 1;
            }
            Ok(Some(chain_hash)) => {
                if *relay_hash == chain_hash {
                    matched += 1;
                } else {
                    mismatched += 1;
                    mismatches.push(serde_json::json!({
                        "slot": slot,
                        "relay_hash": format!("{:#x}", relay_hash),
                        "chain_hash": format!("{:#x}", chain_hash),
                    }));
                    warn!(
                        "Hash mismatch at slot {slot}: relay={:#x} chain={:#x} (possible reorg)",
                        relay_hash, chain_hash
                    );
                }
            }
        }
    }

    let total = by_slot.len();
    let detail = format!(
        "{matched} matched, {mismatched} mismatched, {missed} missed out of {total} delivered"
    );
    let data = serde_json::json!({
        "matched": matched,
        "mismatched": mismatched,
        "missed": missed,
        "mismatches": mismatches,
    });

    if mismatched > 0 {
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
