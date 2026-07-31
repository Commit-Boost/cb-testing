//! Verification report formatting: terminal (colored) and JSON output.

use colored::Colorize;
use serde::Serialize;

use crate::checks::{CheckResult, CheckStatus};

/// Observation window: slot range that was verified.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationWindow {
    pub start_slot: u64,
    pub end_slot: u64,
}

/// Full verification report.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub enclave: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_window: Option<ObservationWindow>,
    pub result: CheckStatus,
    pub checks: Vec<CheckResult>,
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

/// Determine process exit code from the report.
///
/// - 0: all tier-1 checks passed
/// - 1: any tier-1 check failed
/// - 2: setup/discovery failure (no tier-1 checks ran)
pub fn exit_code(report: &VerificationReport) -> i32 {
    let tier1: Vec<_> = report.checks.iter().filter(|c| c.tier == 1).collect();
    if tier1.is_empty() {
        return 2;
    }
    if tier1.iter().any(|c| c.status == CheckStatus::Fail) {
        return 1;
    }
    0
}
