//! Beacon chain health verification checks.

use std::process::Command;

use crate::beacon::BeaconClient;
use crate::checks::CheckResult;

/// Check if the beacon chain has finalized past epoch 2.
pub async fn check_finality(beacon: &BeaconClient) -> CheckResult {
    match beacon.get_finalized_epoch().await {
        Ok(epoch) if epoch >= 2 => {
            CheckResult::pass("chain_finality", 1, format!("Finalized epoch: {epoch}"))
                .with_data(serde_json::json!({ "finalized_epoch": epoch }))
        }
        Ok(epoch) => CheckResult::fail(
            "chain_finality",
            1,
            format!("Finalized epoch too low: {epoch} (need >= 2)"),
        )
        .with_data(serde_json::json!({ "finalized_epoch": epoch })),
        Err(e) => CheckResult::fail("chain_finality", 1, format!("Error checking finality: {e}")),
    }
}

/// Check missed slot rate over a range of slots.
pub async fn check_missed_slots(
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
    threshold: f64,
) -> CheckResult {
    if start_slot > end_slot {
        return CheckResult::fail("missed_slots", 2, "Invalid slot range");
    }
    // Single-slot window: nothing meaningful to check
    if start_slot == end_slot {
        return CheckResult::skip(
            "missed_slots",
            2,
            format!(
                "Single-slot window (slot {}), skipping missed slot check",
                start_slot
            ),
        );
    }
    let total = end_slot - start_slot;

    let mut missed = 0u64;
    for slot in start_slot..end_slot {
        match beacon.get_header(slot).await {
            Ok(None) => missed += 1,
            Err(_) => missed += 1,
            Ok(Some(_)) => {}
        }
    }

    let rate = missed as f64 / total as f64;
    let data = serde_json::json!({
        "missed": missed,
        "total": total,
        "rate": (rate * 10000.0).round() / 10000.0,
        "threshold": threshold,
        "start_slot": start_slot,
        "end_slot": end_slot,
    });

    if rate < threshold {
        CheckResult::pass(
            "missed_slots",
            2,
            format!(
                "Missed {missed}/{total} slots ({:.2}%), under {:.0}% threshold",
                rate * 100.0,
                threshold * 100.0
            ),
        )
        .with_data(data)
    } else {
        CheckResult::warn(
            "missed_slots",
            2,
            format!(
                "Missed {missed}/{total} slots ({:.2}%), above {:.0}% threshold",
                rate * 100.0,
                threshold * 100.0
            ),
        )
        .with_data(data)
    }
}

/// Check if the beacon node is done syncing.
pub async fn check_sync_status(beacon: &BeaconClient) -> CheckResult {
    match beacon.is_syncing().await {
        Ok(false) => CheckResult::pass("sync_status", 1, "Node is fully synced"),
        Ok(true) => CheckResult::fail("sync_status", 1, "Node is still syncing"),
        Err(e) => CheckResult::fail("sync_status", 1, format!("Error checking sync: {e}")),
    }
}

/// Check if commit-boost services are running in the enclave.
///
/// Uses `kurtosis enclave inspect` and greps for services matching the pattern.
pub fn check_cb_running(enclave: &str, service_pattern: &str) -> CheckResult {
    let output = match Command::new("kurtosis")
        .args(["enclave", "inspect", enclave])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return CheckResult::fail("cb_running", 1, format!("kurtosis CLI error: {e}"));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return CheckResult::fail(
            "cb_running",
            1,
            format!("Cannot inspect enclave '{enclave}': {}", stderr.trim()),
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pat_lc = service_pattern.to_lowercase();
    let cb_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.to_lowercase().contains(&pat_lc))
        .collect();
    let running: Vec<&&str> = cb_lines
        .iter()
        .filter(|l| l.to_lowercase().contains("running"))
        .collect();

    if !running.is_empty() {
        CheckResult::pass(
            "cb_running",
            1,
            format!(
                "Found {} {service_pattern} service(s) running",
                running.len()
            ),
        )
    } else if !cb_lines.is_empty() {
        CheckResult::fail(
            "cb_running",
            1,
            format!(
                "Found {} {service_pattern} service(s) but none running",
                cb_lines.len()
            ),
        )
    } else {
        CheckResult::fail(
            "cb_running",
            1,
            format!("No {service_pattern} services found in enclave '{enclave}'"),
        )
    }
}

/// Run all chain health checks.
///
/// Finalization check (`chain_finality`) is included only when:
/// - `end_slot >= 96` (epoch 3+ — justification cascade has time to finalize epoch 2)
/// - `skip_finalization` is `false`
///
/// Otherwise the check is skipped with a reason.
pub async fn run_chain_health_checks(
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
    enclave: &str,
    skip_finalization: bool,
) -> Vec<CheckResult> {
    // Finalization needs ~3 epochs from genesis for the justification
    // cascade to finalize epoch 2. Skip if the observation window ends
    // before slot 96 (end of epoch 3).
    const FINALITY_POSSIBLE_AFTER_SLOT: u64 = 96;

    let mut checks = vec![
        check_missed_slots(beacon, start_slot, end_slot, 0.10).await,
        check_sync_status(beacon).await,
        check_cb_running(enclave, "commit-boost"),
    ];

    if skip_finalization {
        checks.insert(
            0,
            CheckResult::skip(
                "chain_finality",
                1,
                "Finalization check skipped (--skip-finalization-check)",
            ),
        );
    } else if end_slot < FINALITY_POSSIBLE_AFTER_SLOT {
        checks.insert(
            0,
            CheckResult::skip(
                "chain_finality",
                1,
                format!(
                    "Finalization not expected yet (observation ends at slot {end_slot}, need >= {FINALITY_POSSIBLE_AFTER_SLOT})"
                ),
            ),
        );
    } else {
        checks.insert(0, check_finality(beacon).await);
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckStatus;

    // A BeaconClient constructor is pure (it only builds a reqwest::Client, no
    // I/O). The check_missed_slots guard branches below return BEFORE any await,
    // so this client is never used for a request — the tests exercise pure
    // decision logic, no devnet required.
    fn dummy_beacon() -> BeaconClient {
        BeaconClient::new("http://127.0.0.1:0")
    }

    // Contract: an inverted slot range (start > end) is nonsense input and must
    // FAIL fast without consulting the beacon.
    #[tokio::test]
    async fn missed_slots_inverted_range_fails() {
        let beacon = dummy_beacon();
        let r = check_missed_slots(&beacon, 10, 5, 0.10).await;
        assert_eq!(r.status, CheckStatus::Fail);
        assert_eq!(r.id, "missed_slots");
        assert_eq!(r.tier, 2);
    }

    // Contract: a single-slot window (start == end) has no interior slots to
    // measure a miss rate over, so it must SKIP (not silently PASS on zero
    // data — that would be a false green).
    #[tokio::test]
    async fn missed_slots_single_slot_window_skips() {
        let beacon = dummy_beacon();
        let r = check_missed_slots(&beacon, 5, 5, 0.10).await;
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(r.id, "missed_slots");
        assert_eq!(r.tier, 2);
        assert!(r.detail.contains("Single-slot"));
    }
}
