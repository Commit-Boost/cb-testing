//! Verification report formatting: terminal (colored) and JSON output.

use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::checks::{CheckResult, CheckStatus};

/// Observation window: slot range that was verified.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationWindow {
    pub start_slot: u64,
    pub end_slot: u64,
}

/// A single Docker image the run used, with its resolved image ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRef {
    /// Role of the image in the pipeline (helix_relay, mev_boost, ...).
    pub role: String,
    /// Image name/tag as configured (e.g. `ghcr.io/gattaca-com/helix-relay:main`).
    pub name: String,
    /// Resolved Docker image ID (`sha256:...`), or `null` when the image was
    /// not present locally / could not be inspected. Deliberately serialized as
    /// `null` (not omitted) so a report records that the image was unresolved.
    #[serde(default)]
    pub id: Option<String>,
}

/// Provenance: WHAT was tested. Makes a report self-describing so two runs can
/// be compared for regression detection (consumed by `sim diff`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// Path to the Kurtosis config the run used (as passed on the CLI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// Short content fingerprint of the config file bytes (first 12 hex of a
    /// std `DefaultHasher`). `None` if no config was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    /// Resolved Docker image IDs for the images the devnet ran.
    #[serde(default)]
    pub images: Vec<ImageRef>,
}

/// Full verification report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub enclave: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_window: Option<ObservationWindow>,
    pub result: CheckStatus,
    pub checks: Vec<CheckResult>,
    /// What was tested (config + resolved image IDs). Best-effort: `None` when
    /// docker is unreachable or the config could not be parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// Print the report to stdout in terminal-colored format.
pub fn print_terminal(report: &VerificationReport) {
    println!(
        "{}",
        format!("Verification Report: {}", report.enclave).bold()
    );
    println!("Timestamp: {}", report.timestamp);
    if let Some(ref w) = report.observation_window {
        println!(
            "Observation window: slot {} -> {}",
            w.start_slot, w.end_slot
        );
    }
    println!();

    let (mut passed, mut failed, mut warnings, mut _skipped) = (0u32, 0u32, 0u32, 0u32);

    for c in &report.checks {
        let tag = match c.status {
            CheckStatus::Pass => {
                passed += 1;
                "PASS".green().to_string()
            }
            CheckStatus::Fail => {
                failed += 1;
                "FAIL".red().to_string()
            }
            CheckStatus::Warn => {
                warnings += 1;
                "WARN".yellow().to_string()
            }
            CheckStatus::Skip => {
                _skipped += 1;
                "SKIP".dimmed().to_string()
            }
        };
        println!("  [{}] {} - {}", tag, c.id, c.detail);

        if c.status == CheckStatus::Fail
            && !c.data.is_null()
            && let serde_json::Value::Object(map) = &c.data
        {
            for (k, v) in map {
                println!("         {}: {}", k, v);
            }
        }
    }

    println!();
    let overall = match report.result {
        CheckStatus::Pass => "PASS".green().bold().to_string(),
        CheckStatus::Fail => "FAIL".red().bold().to_string(),
        _ => format!("{}", report.result),
    };
    println!(
        "Result: {}  ({} passed, {} failed, {} warnings)",
        overall, passed, failed, warnings
    );
}

/// Print the report to stdout as JSON.
pub fn print_json(report: &VerificationReport) {
    match serde_json::to_string_pretty(report) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Failed to serialize report: {e}"),
    }
}

/// Print report in the requested format.
pub fn print_report(report: &VerificationReport, json_mode: bool) {
    if json_mode {
        print_json(report);
    } else {
        print_terminal(report);
    }
}

/// Save a JSON report to `{output_dir}/{enclave}.json`.
///
/// Only writes if the directory exists (caller must create it).
/// Logs a warning on failure but does not return an error — the
/// verification itself has already completed.
pub fn save_json_report(report: &VerificationReport, output_dir: &str) {
    let report_path = format!(
        "{}/{}.json",
        output_dir.trim_end_matches('/'),
        report.enclave
    );
    match serde_json::to_string_pretty(report) {
        Ok(json) => match std::fs::write(&report_path, &json) {
            Ok(_) => {
                // Intentionally not using tracing here — this module has no
                // tracing dep and adding one would be overkill for a single message.
                eprintln!("Report saved to {report_path}");
            }
            Err(e) => {
                eprintln!("Failed to write report to {report_path}: {e}");
            }
        },
        Err(e) => {
            eprintln!("Failed to serialize report for {report_path}: {e}");
        }
    }
}

/// The single tier-1 failure predicate shared by the report's overall `result`
/// and by [`exit_code`], so the two verdicts can never diverge.
///
/// True iff any tier-1 check has status `Fail`. Note a tier-1 `Skip` is NOT a
/// failure — this is what the C1 fix relies on: it makes the relay check FAIL,
/// it does not lean on exit_code treating Skip as a failure.
pub fn tier1_failed(checks: &[CheckResult]) -> bool {
    checks
        .iter()
        .any(|c| c.tier == 1 && c.status == CheckStatus::Fail)
}

/// Determine process exit code from the report.
///
/// - 0: all tier-1 checks passed
/// - 1: any tier-1 check failed
/// - 2: setup/discovery failure (no tier-1 checks ran)
pub fn exit_code(report: &VerificationReport) -> i32 {
    let has_tier1 = report.checks.iter().any(|c| c.tier == 1);
    if !has_tier1 {
        return 2;
    }
    if tier1_failed(&report.checks) {
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// Provenance: record WHAT was tested (config + resolved Docker image IDs).
// ---------------------------------------------------------------------------

// Baked image defaults — mirror `sim`'s `Images::default()` (that map lives in
// the `sim` binary crate, which this binary can't import). Used when a config
// does not pin the image, or when no config was supplied.
const DEFAULT_HELIX_RELAY_IMAGE: &str = "ghcr.io/gattaca-com/helix-relay:main";
const DEFAULT_MEV_BOOST_IMAGE: &str = "commit-boost/commit-boost:kurtosis";
const DEFAULT_MEV_BUILDER_IMAGE: &str = "ethpandaops/reth-rbuilder:develop";
const DEFAULT_MEV_BUILDER_CL_IMAGE: &str = "sigp/lighthouse:latest";

/// Short content fingerprint of `bytes`: first 12 hex chars of a std
/// `DefaultHasher`. Not cryptographic — a cheap change-detector for `sim diff`.
fn short_hash(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    // 64-bit digest => 16 hex chars; take the leading 12.
    format!("{:016x}", h.finish())[..12].to_string()
}

/// Marker: `docker` itself was unreachable (binary missing / daemon down), as
/// distinct from an image simply not being present locally.
struct DockerUnavailable;

/// Resolve a Docker image name to its ID via `docker image inspect`.
///
/// - `Ok(Some(id))` — image present, ID resolved.
/// - `Ok(None)` — docker works but the image isn't present (=> null in report).
/// - `Err(DockerUnavailable)` — docker binary missing / daemon unreachable.
fn resolve_image_id(image: &str) -> Result<Option<String>, DockerUnavailable> {
    let output = std::process::Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output();

    let out = match output {
        Ok(o) => o,
        // Spawn failed (docker not on PATH) => docker unavailable.
        Err(_) => return Err(DockerUnavailable),
    };

    if out.status.code() == Some(127) {
        return Err(DockerUnavailable);
    }

    if out.status.success() {
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Ok(if id.is_empty() { None } else { Some(id) });
    }

    // Non-zero exit: distinguish "daemon down" (unavailable) from "no such
    // image" (a present-but-empty null, best-effort).
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Cannot connect to the Docker daemon") || stderr.contains("daemon") {
        Err(DockerUnavailable)
    } else {
        Ok(None)
    }
}

/// Gather run provenance: the config path + content hash, and the resolved
/// Docker image IDs for the images the devnet used.
///
/// Best-effort and side-effect-free on failure:
/// - a provided-but-unreadable/unparseable config => `None` (we won't attach
///   misleading default image names to a run that used a bespoke config);
/// - docker unreachable => `None`;
/// - an individual image not present locally => that image's `id` is `null`.
///
/// Image names come from the config's `mev_params.{helix_relay,mev_boost,
/// mev_builder,mev_builder_cl}_image` fields, falling back to the baked defaults.
pub fn gather_provenance(config_path: Option<&str>) -> Option<Provenance> {
    let (mev_params, config_hash) = match config_path {
        Some(path) => {
            let bytes = std::fs::read(path).ok()?;
            let root: serde_yaml::Value = serde_yaml::from_slice(&bytes).ok()?;
            (root.get("mev_params").cloned(), Some(short_hash(&bytes)))
        }
        None => (None, None),
    };

    // (role, config key, baked default)
    let specs = [
        (
            "helix_relay",
            "helix_relay_image",
            DEFAULT_HELIX_RELAY_IMAGE,
        ),
        ("mev_boost", "mev_boost_image", DEFAULT_MEV_BOOST_IMAGE),
        (
            "mev_builder",
            "mev_builder_image",
            DEFAULT_MEV_BUILDER_IMAGE,
        ),
        (
            "mev_builder_cl",
            "mev_builder_cl_image",
            DEFAULT_MEV_BUILDER_CL_IMAGE,
        ),
    ];

    let mut images = Vec::with_capacity(specs.len());
    for (role, key, default) in specs {
        let name = mev_params
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| default.to_string());
        match resolve_image_id(&name) {
            Ok(id) => images.push(ImageRef {
                role: role.to_string(),
                name,
                id,
            }),
            // If docker itself is down we can't trust ANY id — abandon provenance.
            Err(DockerUnavailable) => return None,
        }
    }

    Some(Provenance {
        config_path: config_path.map(str::to_string),
        config_hash,
        images,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;

    /// Build a report from `(tier, status)` pairs — just enough to exercise the
    /// exit-code / result verdict contract.
    fn report_from(checks: Vec<(u8, CheckStatus)>) -> VerificationReport {
        let checks = checks
            .into_iter()
            .enumerate()
            .map(|(i, (tier, status))| {
                let id = format!("check-{i}");
                match status {
                    CheckStatus::Pass => CheckResult::pass(id, tier, "ok"),
                    CheckStatus::Fail => CheckResult::fail(id, tier, "bad"),
                    CheckStatus::Warn => CheckResult::warn(id, tier, "meh"),
                    CheckStatus::Skip => CheckResult::skip(id, tier, "n/a"),
                }
            })
            .collect();
        VerificationReport {
            enclave: "test".to_string(),
            timestamp: "1970-01-01T00:00:00Z".to_string(),
            observation_window: None,
            result: CheckStatus::Pass,
            checks,
            provenance: None,
        }
    }

    /// The interchange contract: a report written to disk by a verification run
    /// must be readable back by `sim diff`. These two live in different crates
    /// (lib writes, the sim bin reads) and are only connected by serde derives,
    /// so nothing but a round-trip proves the pipeline actually composes.
    #[test]
    fn saved_report_round_trips_back_into_a_report() {
        let dir =
            std::env::temp_dir().join(format!("cb-report-rt-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut report = report_from(vec![(1, CheckStatus::Pass), (2, CheckStatus::Warn)]);
        report.enclave = "rt-enclave".to_string();
        report.observation_window = Some(ObservationWindow {
            start_slot: 160,
            end_slot: 224,
        });
        report.provenance = Some(Provenance {
            config_path: Some("configs/generated/cb-basic.yml".to_string()),
            config_hash: Some("abc123def456".to_string()),
            images: vec![ImageRef {
                role: "mev_boost".to_string(),
                name: "commit-boost/commit-boost:kurtosis".to_string(),
                id: Some("sha256:deadbeef".to_string()),
            }],
        });

        save_json_report(&report, dir.to_str().unwrap());

        let path = dir.join("rt-enclave.json");
        let raw =
            std::fs::read_to_string(&path).expect("save_json_report must write <enclave>.json");
        let back: VerificationReport = serde_json::from_str(&raw)
            .expect("a saved report must deserialize (sim diff reads it)");

        assert_eq!(back.enclave, "rt-enclave");
        assert_eq!(back.result, report.result);
        assert_eq!(back.checks.len(), 2);
        assert_eq!(back.observation_window.unwrap().end_slot, 224);
        let prov = back
            .provenance
            .expect("provenance must survive the round trip");
        assert_eq!(prov.config_hash.as_deref(), Some("abc123def456"));
        assert_eq!(prov.images[0].id.as_deref(), Some("sha256:deadbeef"));
        // The wire name is `result`, not `status` - sim diff and any external
        // consumer key on it.
        assert!(
            raw.contains("\"result\""),
            "checks serialize their status as `result`"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_a_no_op_when_the_directory_does_not_exist() {
        // Documented behavior: saving never fails the run, it warns. A verify
        // that has already finished must not die on a bad --output-dir.
        let report = report_from(vec![(1, CheckStatus::Pass)]);
        save_json_report(&report, "/nonexistent/dir/for/cb/report");
        // Reaching here without panicking IS the assertion.
    }

    #[test]
    fn short_hash_is_deterministic_and_fixed_width() {
        let a = short_hash(b"the same bytes");
        let b = short_hash(b"the same bytes");
        let c = short_hash(b"different bytes");
        assert_eq!(a, b, "same input must give the same config_hash");
        assert_ne!(a, c, "different configs must be distinguishable");
        assert_eq!(a.len(), 12, "12 hex chars, as `sim diff` prints them");
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn short_hash_handles_empty_input() {
        assert_eq!(short_hash(b"").len(), 12);
    }

    #[test]
    fn print_report_json_and_terminal_both_run() {
        // Smoke: neither renderer may panic on a report carrying every status,
        // a FAIL with data (the terminal path prints data lines for fails), an
        // observation window and provenance.
        let mut report = report_from(vec![
            (1, CheckStatus::Pass),
            (1, CheckStatus::Fail),
            (2, CheckStatus::Warn),
            (3, CheckStatus::Skip),
        ]);
        report.observation_window = Some(ObservationWindow {
            start_slot: 1,
            end_slot: 2,
        });
        print_report(&report, true);
        print_report(&report, false);
    }

    #[test]
    fn exit_code_no_tier1_checks_is_2() {
        // No tier-1 checks ran at all => setup/discovery failure.
        let report = report_from(vec![(2, CheckStatus::Pass), (3, CheckStatus::Fail)]);
        assert_eq!(exit_code(&report), 2);
    }

    #[test]
    fn exit_code_empty_report_is_2() {
        let report = report_from(vec![]);
        assert_eq!(exit_code(&report), 2);
    }

    #[test]
    fn exit_code_tier1_fail_is_1() {
        let report = report_from(vec![(1, CheckStatus::Pass), (1, CheckStatus::Fail)]);
        assert_eq!(exit_code(&report), 1);
    }

    #[test]
    fn exit_code_all_tier1_non_fail_is_0() {
        // Pass / Warn / Skip on tier-1 checks are all a green verdict.
        let report = report_from(vec![
            (1, CheckStatus::Pass),
            (1, CheckStatus::Warn),
            (1, CheckStatus::Skip),
        ]);
        assert_eq!(exit_code(&report), 0);
    }

    #[test]
    fn exit_code_tier1_skip_alone_is_0() {
        // The subtle one the C1 fix relies on: a tier-1 SKIP does NOT fail the
        // run. exit_code only fails on Fail — so the C1 fix works by making the
        // relay check FAIL, not by changing exit_code's treatment of Skip.
        let report = report_from(vec![(1, CheckStatus::Skip)]);
        assert_eq!(exit_code(&report), 0);
    }

    #[test]
    fn exit_code_ignores_non_tier1_fail_when_tier1_passes() {
        // A tier-1 check exists and passes; a tier-2 Fail must not flip to 1.
        let report = report_from(vec![(1, CheckStatus::Pass), (2, CheckStatus::Fail)]);
        assert_eq!(exit_code(&report), 0);
    }

    #[test]
    fn tier1_failed_matches_exit_code_predicate() {
        // The shared predicate and exit_code agree on the fail case.
        let checks: Vec<CheckResult> = report_from(vec![(1, CheckStatus::Fail)]).checks;
        assert!(tier1_failed(&checks));

        let clean: Vec<CheckResult> = report_from(vec![(1, CheckStatus::Skip)]).checks;
        assert!(!tier1_failed(&clean));
    }
}
