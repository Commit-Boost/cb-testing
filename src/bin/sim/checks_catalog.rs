//! `sim checks --list [--json]` — the machine-readable catalog of what `cb-verify`
//! asserts, so an agent can discover the harness's contract WITHOUT reading
//! `src/checks/*` or `docs/CHECKS.md`.
//!
//! IMPORTANT — this is a STATIC, hand-maintained catalog. It mirrors two sources
//! of truth and MUST be kept in sync with BOTH when a check is added, removed,
//! retiered, or its data-source changes:
//!   * the check ids + tiers in `src/checks/*.rs` (the code that emits them), and
//!   * the per-check contract in `docs/CHECKS.md` (the prose catalog).
//!
//! There is no derivation today (the checks are constructed imperatively across
//! several modules with no single registry to reflect on), so drift is caught by
//! review, not the compiler. If a clean registry is ever introduced in
//! `src/checks`, prefer deriving this from it and delete the hand-maintenance note.

use eyre::Result;
use serde::Serialize;

/// Where a check's evidence comes from. Determines how it behaves when a service
/// dies mid-run (see `docs/CHECKS.md` "Data-source robustness").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DataSource {
    /// Commit-boost PBS container logs (survive a relay crash).
    #[serde(rename = "cb-logs")]
    CbLogs,
    /// Relay data API (`/relay/v1/data/...`) — fragile; dies with the relay.
    #[serde(rename = "relay-data-api")]
    RelayDataApi,
    /// Beacon node HTTP API.
    #[serde(rename = "beacon-api")]
    BeaconApi,
    /// Commit-boost Prometheus metrics (usually absent in default PBS mode).
    #[serde(rename = "cb-prometheus")]
    CbPrometheus,
    /// `kurtosis enclave inspect` (survives a relay crash).
    #[serde(rename = "kurtosis-inspect")]
    KurtosisInspect,
}

impl DataSource {
    /// The stable kebab-case token used in the table + JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            DataSource::CbLogs => "cb-logs",
            DataSource::RelayDataApi => "relay-data-api",
            DataSource::BeaconApi => "beacon-api",
            DataSource::CbPrometheus => "cb-prometheus",
            DataSource::KurtosisInspect => "kurtosis-inspect",
        }
    }
}

/// One entry in the check catalog: the discoverable contract for a single check.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    /// The check id as emitted in the JSON report (`CheckResult.id`).
    pub id: &'static str,
    /// Severity tier: 1 (must / invariant), 2 (should / health), 3 (info).
    pub tier: u8,
    /// One-line statement of what a PASS asserts.
    pub title: &'static str,
    /// Where the evidence comes from.
    pub data_source: DataSource,
    /// True iff this check positively asserts that a scenario's FEATURE codepath
    /// fired (per `docs/CHECKS.md`, only `mux.routing` and `relay.best_bid` do).
    pub feature_asserted: bool,
    /// Terse note on the check's WARN/SKIP/escalation quirks — the gotchas a
    /// consumer must internalize (e.g. a tier-1 anomaly that lands as a non-fatal
    /// WARN, or a tier-2 matrix that escalates to tier 1 on FAIL).
    pub severity_note: &'static str,
}

/// The verdict rule a consumer must internalize, printed as the header and worth
/// stating up front: the process exit code keys ONLY on a tier-1 FAIL.
pub const VERDICT_RULE: &str = "verdict: exit code keys ONLY on a tier-1 FAIL \
(exit 1); WARN/SKIP are non-fatal at every tier; no tier-1 check ran = exit 2. \
Several anomaly detectors report as WARN — gate on each check's `result`, not the \
exit code.";

/// The static catalog. Keep in sync with `src/checks/*` + `docs/CHECKS.md`.
pub fn catalog() -> Vec<CatalogEntry> {
    use DataSource::*;
    vec![
        CatalogEntry {
            id: "chain_finality",
            tier: 1,
            title: "finalized epoch >= 2 (the chain is finalizing)",
            data_source: BeaconApi,
            feature_asserted: false,
            severity_note: "tier-1 FAIL fails the run; SKIPs before epoch 3 or under --skip-finalization-check",
        },
        CatalogEntry {
            id: "sync_status",
            tier: 1,
            title: "the beacon node is done syncing",
            data_source: BeaconApi,
            feature_asserted: false,
            severity_note: "tier-1 FAIL fails the run; no WARN/SKIP state",
        },
        CatalogEntry {
            id: "cb_running",
            tier: 1,
            title: "at least one commit-boost service is running",
            data_source: KurtosisInspect,
            feature_asserted: false,
            severity_note: "tier-1 FAIL fails the run; no WARN/SKIP state",
        },
        CatalogEntry {
            id: "missed_slots",
            tier: 2,
            title: "missed-slot rate < 10% over the window",
            data_source: BeaconApi,
            feature_asserted: false,
            severity_note: "WARN over threshold; non-fatal (SKIP on a single-slot window)",
        },
        CatalogEntry {
            id: "relay.payloads_delivered_multi",
            tier: 1,
            title: "at least one payload delivered across relays",
            data_source: RelayDataApi,
            feature_asserted: false,
            severity_note: "tier-1 FAIL fails the run; SKIP if all relays unreachable",
        },
        CatalogEntry {
            id: "relay.builder_blocks_received",
            tier: 2,
            title: "at least one builder block received by a relay",
            data_source: RelayDataApi,
            feature_asserted: false,
            severity_note: "annotative; non-fatal (SKIP if all relays unreachable)",
        },
        CatalogEntry {
            id: "relay.mev_delivery_rate",
            tier: 2,
            title: "MEV-delivered on-chain block fraction >= 0.30",
            data_source: RelayDataApi,
            feature_asserted: false,
            severity_note: "WARN below threshold; FAIL only if no on-chain blocks — tier 2, non-fatal",
        },
        CatalogEntry {
            id: "relay.validator_registrations",
            tier: 3,
            title: "validators are registered with the relay",
            data_source: RelayDataApi,
            feature_asserted: false,
            severity_note: "informational; OMITTED entirely (not even SKIP) if the pubkey fetch failed",
        },
        CatalogEntry {
            id: "payload_hash_match",
            tier: 1,
            title: "relay-delivered hashes match on-chain, no cross-relay conflict",
            data_source: RelayDataApi,
            feature_asserted: false,
            severity_note: "ANOMALY (reorg / relay equivocation) is reported as WARN — NON-FATAL despite tier 1; gate on JSON, not exit code",
        },
        CatalogEntry {
            id: "relay.best_bid",
            tier: 2,
            title: "CB delivered >= the best per-relay bid it was offered",
            data_source: CbLogs,
            feature_asserted: true,
            severity_note: "feature check (cross-relay bid aggregation); suboptimal delivery is WARN; SKIP with < 2 relays",
        },
        CatalogEntry {
            id: "mux.routing",
            tier: 1,
            title: "every checked getHeader routed per the [[mux]] config",
            data_source: CbLogs,
            feature_asserted: true,
            severity_note: "feature check; misrouting FAILs (fatal), but unverifiable routing is WARN — needs CB [logs.stdout] level = debug",
        },
        CatalogEntry {
            id: "feature.timing_games",
            tier: 1,
            title: "timing-games codepath fired (TG: debug logs seen)",
            data_source: CbLogs,
            feature_asserted: true,
            severity_note: "emitted only when the config enables enable_timing_games; PASS on >=1 TG: log line, WARN (non-fatal) if none seen",
        },
        CatalogEntry {
            id: "feature.extra_validation",
            tier: 1,
            title: "extra-validation codepath fired (parent-block fetch logs seen)",
            data_source: CbLogs,
            feature_asserted: true,
            severity_note: "emitted only when the config enables extra_validation_enabled; PASS on >=1 'fetched parent block' log, WARN (non-fatal) if none",
        },
        CatalogEntry {
            id: "feature.skip_sigverify",
            tier: 1,
            title: "skip-sigverify codepath fired (differential via wrong-pubkey relay)",
            data_source: CbLogs,
            feature_asserted: true,
            severity_note: "emitted only when skip_sigverify is enabled; PASS only in the cb-sigverify-diff scenario (wrong-pubkey relay url + >=1 auction winner proves the skip); plain scenarios stay an honest WARN (negative codepath, no positive signal)",
        },
        CatalogEntry {
            id: "cb_get_header_matrix",
            tier: 2,
            title: "get_header status-code distribution healthy",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "tier 2 -> ESCALATES to tier 1 on FAIL (relay 5xx over the 25% rate); CB client-side codes (555 timeout, 556 ws transport) bucket separately and WARN only; SKIP if metrics absent (the default)",
        },
        CatalogEntry {
            id: "cb_register_validator_matrix",
            tier: 2,
            title: "register_validator acceptance healthy",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "tier 2 -> ESCALATES to tier 1 on FAIL (5xx); SKIP if metrics absent",
        },
        CatalogEntry {
            id: "cb_submit_blinded_block_matrix",
            tier: 2,
            title: "at least one blinded-block delivery (200/202)",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "tier 2 -> ESCALATES to tier 1 on FAIL (5xx); SKIP if metrics absent",
        },
        CatalogEntry {
            id: "cb_status_matrix",
            tier: 2,
            title: "status endpoint answering 200",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "tier 2 -> ESCALATES to tier 1 on FAIL (5xx); SKIP if no 200s / metrics absent",
        },
        CatalogEntry {
            id: "cb_relay_v2_unsupported",
            tier: 2,
            title: "no v2 submit_block lost to a relay that 404s the v2 route",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "tier 2 -> ESCALATES to tier 1 on FAIL: every builder block the proposer chose is LOST (CB will not downgrade v2->v1). Usually the relay's route config, not a capability gap (helix needs GetPayloadV2 in enabled_routes); SKIP/PASS if metrics absent",
        },
        CatalogEntry {
            id: "cb_v2_fallback",
            tier: 2,
            title: "no v2->v1 submitBlindedBlock fallbacks",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "WARN on any fallback; never FAIL; SKIP if metrics absent",
        },
        CatalogEntry {
            id: "cb_relay_latency",
            tier: 2,
            title: "p95 relay latency < 500 ms",
            data_source: CbPrometheus,
            feature_asserted: false,
            severity_note: "WARN over threshold; SKIP if the histogram is absent / degenerate",
        },
    ]
}

/// Entry point for `sim checks --list [--json]`.
///
/// `--list` emits the catalog (a readable table, or JSON with `--json`). Without
/// `--list` there is nothing else to do, so it points the caller at the flag.
pub fn run(list: bool, json: bool) -> Result<()> {
    if !list {
        println!(
            "sim checks: pass --list to emit the check catalog (add --json for machine output)."
        );
        return Ok(());
    }
    let entries = catalog();
    if json {
        // Wrap the array in an envelope so the verdict rule travels WITH the data.
        let doc = serde_json::json!({
            "verdict_rule": VERDICT_RULE,
            "checks": entries,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        print_table(&entries);
    }
    Ok(())
}

/// Render the catalog as an aligned, human-readable table.
fn print_table(entries: &[CatalogEntry]) {
    println!("cb-verify check catalog ({} checks)", entries.len());
    println!("{VERDICT_RULE}");
    println!();

    let id_w = entries.iter().map(|e| e.id.len()).max().unwrap_or(2).max(2);
    let src_w = entries
        .iter()
        .map(|e| e.data_source.as_str().len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:<id_w$}  {:>4}  {:>4}  {:<src_w$}  asserts",
        "id", "tier", "feat", "source",
    );
    for e in entries {
        println!(
            "{:<id_w$}  {:>4}  {:>4}  {:<src_w$}  {}",
            e.id,
            e.tier,
            if e.feature_asserted { "yes" } else { "-" },
            e.data_source.as_str(),
            e.title,
        );
    }
    println!();
    println!("severity notes:");
    for e in entries {
        println!("  {}: {}", e.id, e.severity_note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_non_empty_and_every_tier_is_valid() {
        let entries = catalog();
        assert!(!entries.is_empty(), "catalog must not be empty");
        for e in &entries {
            assert!(
                (1..=3).contains(&e.tier),
                "check {:?} has invalid tier {}",
                e.id,
                e.tier
            );
            assert!(!e.id.is_empty(), "an entry has an empty id");
            assert!(!e.title.is_empty(), "check {:?} has an empty title", e.id);
            assert!(
                !e.severity_note.is_empty(),
                "check {:?} has an empty severity note",
                e.id
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let entries = catalog();
        let mut ids: Vec<&str> = entries.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids, deduped, "catalog has duplicate check ids");
    }

    #[test]
    fn only_the_feature_checks_assert_a_feature() {
        // Guards the CHECKS.md invariant: exactly these checks positively assert
        // that a scenario's feature codepath fired — the two runtime-behaviour
        // checks (best_bid, mux.routing) plus the three Law-3 feature-fired
        // checks (skip_sigverify is a WARN-only honest report).
        let feature_ids: Vec<&str> = catalog()
            .iter()
            .filter(|e| e.feature_asserted)
            .map(|e| e.id)
            .collect();
        assert_eq!(
            feature_ids,
            vec![
                "relay.best_bid",
                "mux.routing",
                "feature.timing_games",
                "feature.extra_validation",
                "feature.skip_sigverify",
            ]
        );
    }

    #[test]
    fn json_envelope_carries_the_verdict_rule() {
        let doc = serde_json::json!({
            "verdict_rule": VERDICT_RULE,
            "checks": catalog(),
        });
        let s = serde_json::to_string(&doc).unwrap();
        assert!(s.contains("tier-1 FAIL"), "verdict rule must be present");
        assert!(s.contains("chain_finality"), "checks must serialize");
        assert!(
            s.contains("relay-data-api"),
            "data-source token must serialize"
        );
    }
}
