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
    let total = end_slot.saturating_sub(start_slot);
    if total == 0 {
        return CheckResult::fail("missed_slots", 2, "Invalid slot range");
    }

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
pub async fn run_chain_health_checks(
    beacon: &BeaconClient,
    start_slot: u64,
    end_slot: u64,
    enclave: &str,
) -> Vec<CheckResult> {
    vec![
        check_finality(beacon).await,
        check_missed_slots(beacon, start_slot, end_slot, 0.10).await,
        check_sync_status(beacon).await,
        check_cb_running(enclave, "commit-boost"),
    ]
}
