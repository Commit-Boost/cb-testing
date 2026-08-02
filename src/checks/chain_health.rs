//! Beacon chain health verification checks.

use std::process::Command;

use crate::beacon::BeaconClient;
use crate::checks::CheckResult;

/// Classify a finalized epoch: >= 2 PASSes, anything lower FAILs.
///
/// Pure decision core extracted from [`check_finality`] so it can be unit
/// tested without a live beacon (the P3 Law-4 pattern — see
/// `cb_metrics::classify_endpoint`).
pub fn classify_finality(finalized_epoch: u64) -> CheckResult {
    let data = serde_json::json!({ "finalized_epoch": finalized_epoch });
    if finalized_epoch >= 2 {
        CheckResult::pass(
            "chain_finality",
            1,
            format!("Finalized epoch: {finalized_epoch}"),
        )
        .with_data(data)
    } else {
        CheckResult::fail(
            "chain_finality",
            1,
            format!("Finalized epoch too low: {finalized_epoch} (need >= 2)"),
        )
        .with_data(data)
    }
}

/// Check if the beacon chain has finalized past epoch 2.
///
/// Thin IO wrapper: fetches the finalized epoch, then defers to
/// [`classify_finality`].
pub async fn check_finality(beacon: &BeaconClient) -> CheckResult {
    match beacon.get_finalized_epoch().await {
        Ok(epoch) => classify_finality(epoch),
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

    // Attach the slot bounds to the classifier's data payload (they're context
    // the pure rate logic doesn't need to reach a verdict, but the report does).
    let mut result = classify_missed_slots(missed, total, threshold);
    if let Some(obj) = result.data.as_object_mut() {
        obj.insert("start_slot".to_string(), serde_json::json!(start_slot));
        obj.insert("end_slot".to_string(), serde_json::json!(end_slot));
    }
    result
}

/// Classify a miss rate: strictly BELOW `threshold` PASSes, at-or-above WARNs.
///
/// Pure decision core extracted from [`check_missed_slots`] (the boundary is
/// intentionally strict — `rate < threshold` — so a rate exactly at the
/// threshold warns). Callers guarantee `total > 0` (the inverted-range and
/// single-slot windows are handled upstream before any measurement).
pub fn classify_missed_slots(missed: u64, total: u64, threshold: f64) -> CheckResult {
    let rate = missed as f64 / total as f64;
    let data = serde_json::json!({
        "missed": missed,
        "total": total,
        "rate": (rate * 10000.0).round() / 10000.0,
        "threshold": threshold,
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
    classify_cb_running(&stdout, service_pattern)
}

/// Classify `kurtosis enclave inspect` stdout for a service pattern.
///
/// Pure string logic extracted from [`check_cb_running`]:
/// - at least one matching line marked `running` => PASS
/// - matching line(s) exist but none running => FAIL
/// - no matching line at all => FAIL
///
/// Case-insensitive on both the pattern and the `running` marker.
pub fn classify_cb_running(inspect_stdout: &str, service_pattern: &str) -> CheckResult {
    let pat_lc = service_pattern.to_lowercase();
    let cb_lines: Vec<&str> = inspect_stdout
        .lines()
        .filter(|l| l.to_lowercase().contains(&pat_lc))
        .collect();
    let running = cb_lines
        .iter()
        .filter(|l| l.to_lowercase().contains("running"))
        .count();

    if running > 0 {
        CheckResult::pass(
            "cb_running",
            1,
            format!("Found {running} {service_pattern} service(s) running"),
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
            format!("No {service_pattern} services found"),
        )
    }
}

/// Run all chain health checks.
///
/// Finalization check (`chain_finality`) is included only when:
/// - `end_slot >= 160` (epoch 5 — first slot where `finalized_epoch >= 2` is
///   reliably reached; see [`classify_finality`] for the >= 2 threshold)
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
    // The finality check demands `finalized_epoch >= 2`, but finality lags the
    // chain head by ~2 epochs: epoch N justifies at N+1 and finalizes at N+2.
    // So epoch 2 does not finalize until the END of epoch 4 (~slot 160), which
    // is the first slot where `finalized_epoch >= 2` is reliably reached.
    // Gating on the old value 96 (end of epoch 3, where only epoch 1 has
    // finalized) ran the >= 2 check too early and FAILed healthy chains whose
    // window ended in ~[96, 160). Skip until epoch 5 (5 * 32) so the demanded
    // finalization has actually had time to happen.
    const FINALITY_POSSIBLE_AFTER_SLOT: u64 = 5 * 32;

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

    // --- classify_finality (seam 1) -------------------------------------

    // Contract: finalized epoch >= 2 is the healthy state and PASSes.
    #[test]
    fn finality_at_threshold_passes() {
        let r = classify_finality(2);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.id, "chain_finality");
        assert_eq!(r.tier, 1);
        assert_eq!(r.data["finalized_epoch"], 2);
    }

    #[test]
    fn finality_above_threshold_passes() {
        let r = classify_finality(7);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["finalized_epoch"], 7);
    }

    // Contract: finalized epoch < 2 FAILs, and the epoch is surfaced in data.
    #[test]
    fn finality_below_threshold_fails() {
        let r = classify_finality(1);
        assert_eq!(r.status, CheckStatus::Fail);
        assert_eq!(r.id, "chain_finality");
        assert!(r.detail.contains("too low"));
        assert_eq!(r.data["finalized_epoch"], 1);
    }

    #[test]
    fn finality_zero_fails() {
        let r = classify_finality(0);
        assert_eq!(r.status, CheckStatus::Fail);
        assert_eq!(r.data["finalized_epoch"], 0);
    }

    // --- classify_missed_slots (seam 2) ---------------------------------

    // Contract: a miss rate strictly BELOW threshold PASSes.
    #[test]
    fn missed_slots_below_threshold_passes() {
        // 5/100 = 5% < 10%
        let r = classify_missed_slots(5, 100, 0.10);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.id, "missed_slots");
        assert_eq!(r.tier, 2);
        assert_eq!(r.data["missed"], 5);
        assert_eq!(r.data["total"], 100);
    }

    // Contract: a miss rate exactly AT threshold is not "under" it, so it WARNs
    // (the boundary belongs to the warn side — `rate < threshold` is strict).
    #[test]
    fn missed_slots_at_threshold_warns() {
        // 10/100 = 10% == 10%
        let r = classify_missed_slots(10, 100, 0.10);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("above"));
    }

    // Contract: a miss rate ABOVE threshold WARNs.
    #[test]
    fn missed_slots_above_threshold_warns() {
        // 25/100 = 25% > 10%
        let r = classify_missed_slots(25, 100, 0.10);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.data["missed"], 25);
    }

    // --- classify_cb_running (seam 3) -----------------------------------

    // A minimal `kurtosis enclave inspect` snippet with a running CB service.
    const INSPECT_RUNNING: &str = "\
========================================== User Services ==========================================
UUID           Name                          Ports                          Status
abc123         cl-1-lighthouse-geth          http: 4000/tcp -> ...          RUNNING
def456         commit-boost-pbs              api: 18550/tcp -> ...          RUNNING
";

    const INSPECT_STOPPED: &str = "\
UUID           Name                          Ports                          Status
abc123         cl-1-lighthouse-geth          http: 4000/tcp -> ...          RUNNING
def456         commit-boost-pbs              api: 18550/tcp -> ...          STOPPED
";

    const INSPECT_NONE: &str = "\
UUID           Name                          Ports                          Status
abc123         cl-1-lighthouse-geth          http: 4000/tcp -> ...          RUNNING
";

    // Contract: at least one matching line marked RUNNING => PASS.
    #[test]
    fn cb_running_present_passes() {
        let r = classify_cb_running(INSPECT_RUNNING, "commit-boost");
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.id, "cb_running");
        assert_eq!(r.tier, 1);
        assert!(r.detail.contains("running"));
    }

    // Contract: a matching service exists but none is RUNNING => FAIL.
    #[test]
    fn cb_running_found_but_stopped_fails() {
        let r = classify_cb_running(INSPECT_STOPPED, "commit-boost");
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.contains("none running"));
    }

    // Contract: no matching service line at all => FAIL.
    #[test]
    fn cb_running_none_found_fails() {
        let r = classify_cb_running(INSPECT_NONE, "commit-boost");
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.contains("No"));
    }
}
