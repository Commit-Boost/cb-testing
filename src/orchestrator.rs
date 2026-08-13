//! cb-orchestrator: Run multiple Commit-Boost config scenarios concurrently.
//!
//! Replaces `just test-all` with a concurrent pipeline:
//!
//!   Launch enclaves ──► Wait for readiness ──► Observe ──► Check ──► Tear down
//!        │                    │                   │          │          │
//!        └────────────────────┴───────────────────┴──────────┴──────────┘
//!        All enclaves run through the pipeline concurrently (bounded by --jobs)
//!
//! Each enclave is independent. While one is observing, another can be launching.
//! The bottleneck is the observation window (~2 epochs ≈ 12 min), so with --jobs=4
//! you get roughly 4× throughput vs sequential `just test-all`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::Parser;
use eyre::{Context, Result, bail};
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

// For colored output in print_batch_summary
use colored::Colorize;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "cb-orchestrator", version, about)]
struct Cli {
    /// Config files to run (YAML Kurtosis configs).
    ///
    /// Accepts individual files or directories (all *.yml in directory).
    /// If omitted, defaults to configs/generated/*.yml.
    #[arg(value_name = "CONFIG")]
    configs: Vec<PathBuf>,

    /// Max concurrent enclaves (default: 2).
    ///
    /// Each enclave uses ~2-4 GB RAM and 2-4 CPU cores. A 16-core/32GB
    /// machine can comfortably run 4-6 concurrent enclaves.
    #[arg(long, default_value_t = 2)]
    jobs: usize,

    /// Kurtosis package path or ref.
    #[arg(long, default_value = "./ethereum-package")]
    package: String,

    /// Observation window in epochs.
    #[arg(long, default_value_t = 2)]
    min_epochs: u64,

    /// Wait until this epoch before starting observation.
    #[arg(long, default_value_t = 7)]
    target_epoch: u64,

    /// Readiness timeout in seconds.
    #[arg(long, default_value_t = 3600)]
    timeout: u64,

    /// Save JSON reports to this directory.
    #[arg(long)]
    results_dir: Option<PathBuf>,

    /// Keep enclaves running after checks (don't tear down).
    #[arg(long)]
    keep: bool,

    /// Strict mode: promote WARN to FAIL.
    #[arg(long)]
    strict: bool,

    /// Live metrics polling during observation.
    #[arg(long)]
    live_metrics: bool,

    /// Verbose logging.
    #[arg(short, long)]
    verbose: bool,

    /// Skip the chain-finalization check (pass `--skip-finalization-check` to
    /// cb-verify). Use with a low `--target-epoch` for a fast MEV-delivery gate
    /// that does not wait for the chain to finalize (~epoch 4+).
    #[arg(long)]
    skip_finalization: bool,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Per-enclave status tracked by the orchestrator.
#[derive(Debug, Clone)]
struct EnclaveStatus {
    name: String,
    config: PathBuf,
    /// Set when the enclave process has been launched.
    launched_at: Option<Instant>,
    /// Set when the enclave becomes ready for observation.
    ready_at: Option<Instant>,
    /// Set when observation completes.
    observed_at: Option<Instant>,
    /// Set when checks complete.
    checked_at: Option<Instant>,
    /// Check results (populated after Done).
    check_result: Option<CheckSummary>,
}

/// Summarized check result for the final report.
#[derive(Debug, Clone, Serialize)]
struct CheckSummary {
    enclave: String,
    config: String,
    result: String,
    passed: usize,
    failed: usize,
    warnings: usize,
    skipped: usize,
    duration_secs: u64,
}

/// Final batch report.
#[derive(Debug, Serialize)]
struct BatchReport {
    timestamp: String,
    total: usize,
    passed: usize,
    failed: usize,
    results: Vec<CheckSummary>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    // Initialize tracing
    let filter = if cli.verbose {
        "debug,hyper=info,reqwest=info,rustls=info"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // Resolve config files
    let configs = resolve_configs(&cli.configs)?;
    if configs.is_empty() {
        bail!("No config files found. Pass config files or directories as arguments.");
    }

    info!(
        "Orchestrator: {} config(s), {} concurrent job(s)",
        configs.len(),
        cli.jobs
    );

    // Create results dir if needed
    if let Some(ref dir) = cli.results_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Build enclave names from config filenames
    let enclaves: Vec<EnclaveStatus> = configs
        .iter()
        .map(|config| {
            let name = enclave_name(config);
            EnclaveStatus {
                name,
                config: config.clone(),
                launched_at: None,
                ready_at: None,
                observed_at: None,
                checked_at: None,
                check_result: None,
            }
        })
        .collect();

    // Clean up any stale enclaves with the same names
    for enc in &enclaves {
        info!("Cleaning stale enclave '{}' (if any)...", enc.name);
        let _ = Command::new("kurtosis")
            .args(["enclave", "rm", "-f", &enc.name])
            .output();
    }

    // Semaphore to bound concurrency
    let semaphore = std::sync::Arc::new(Semaphore::new(cli.jobs));

    // Spawn all enclave pipelines concurrently
    let mut join_set = JoinSet::new();

    for (idx, enc) in enclaves.into_iter().enumerate() {
        let sem = semaphore.clone();
        let package = cli.package.clone();
        let results_dir = cli.results_dir.clone();
        let min_epochs = cli.min_epochs;
        let target_epoch = cli.target_epoch;
        let timeout = cli.timeout;
        let keep = cli.keep;
        let strict = cli.strict;
        let live_metrics = cli.live_metrics;
        let verbose = cli.verbose;
        let skip_finalization = cli.skip_finalization;

        join_set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let result = run_enclave_pipeline(
                enc,
                &package,
                min_epochs,
                target_epoch,
                timeout,
                keep,
                strict,
                live_metrics,
                verbose,
                skip_finalization,
                results_dir.as_deref(),
            )
            .await;
            (idx, result)
        });
    }

    // Collect results
    let mut results: Vec<(usize, Result<EnclaveStatus>)> = Vec::new();
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok((idx, result)) => results.push((idx, result)),
            Err(e) => {
                error!("Task panicked: {e}");
                results.push((usize::MAX, Err(eyre::eyre!("Task panicked: {e}"))));
            }
        }
    }

    // Sort by original index for deterministic output
    results.sort_by_key(|(idx, _)| *idx);

    // Build summaries
    let mut summaries: Vec<CheckSummary> = Vec::new();
    let mut total_passed = 0usize;
    let mut total_failed = 0usize;

    for (_, result) in &results {
        match result {
            Ok(enc) => {
                if let Some(ref summary) = enc.check_result {
                    if summary.result == "PASS" {
                        total_passed += 1;
                    } else {
                        total_failed += 1;
                    }
                    summaries.push(summary.clone());
                } else {
                    total_failed += 1;
                    summaries.push(CheckSummary {
                        enclave: enc.name.clone(),
                        config: enc.config.display().to_string(),
                        result: "FAILED".to_string(),
                        passed: 0,
                        failed: 0,
                        warnings: 0,
                        skipped: 0,
                        duration_secs: 0,
                    });
                }
            }
            Err(e) => {
                total_failed += 1;
                summaries.push(CheckSummary {
                    enclave: "unknown".to_string(),
                    config: "unknown".to_string(),
                    result: format!("ERROR: {e}"),
                    passed: 0,
                    failed: 0,
                    warnings: 0,
                    skipped: 0,
                    duration_secs: 0,
                });
            }
        }
    }

    // Print batch summary
    let batch = BatchReport {
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        total: summaries.len(),
        passed: total_passed,
        failed: total_failed,
        results: summaries,
    };

    print_batch_summary(&batch);

    // Save batch report if requested
    if let Some(ref dir) = cli.results_dir {
        let report_path = dir.join("batch-report.json");
        match serde_json::to_string_pretty(&batch) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&report_path, &json) {
                    warn!("Failed to write batch report: {e}");
                } else {
                    info!("Batch report saved to {}", report_path.display());
                }
            }
            Err(e) => warn!("Failed to serialize batch report: {e}"),
        }
    }

    // Exit code: 0 if all passed, 1 if any failed
    if total_failed > 0 {
        std::process::exit(1);
    }
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Per-enclave pipeline
// ---------------------------------------------------------------------------

// Launcher fn: threading these as individual params reads clearer than a
// bespoke options struct that exists only for this one call site.
#[allow(clippy::too_many_arguments)]
async fn run_enclave_pipeline(
    mut enc: EnclaveStatus,
    package: &str,
    min_epochs: u64,
    target_epoch: u64,
    timeout: u64,
    keep: bool,
    strict: bool,
    live_metrics: bool,
    verbose: bool,
    skip_finalization: bool,
    results_dir: Option<&Path>,
) -> Result<EnclaveStatus> {
    let start = Instant::now();

    // Phase 1: Launch
    info!(
        "[{}] Launching enclave with config {}...",
        enc.name,
        enc.config.display()
    );
    enc.launched_at = Some(Instant::now());

    if let Err(e) = launch_enclave(&enc.name, &enc.config, package).await {
        let msg = format!("Launch failed: {e}");
        error!("[{}] {}", enc.name, msg);
        // Try to clean up
        if !keep {
            let _ = teardown_enclave(&enc.name);
        }
        bail!(msg);
    }

    // Phase 2: Wait for readiness
    info!(
        "[{}] Waiting for readiness (target epoch {target_epoch})...",
        enc.name
    );

    if let Err(e) = wait_for_enclave_readiness(&enc.name, target_epoch, timeout).await {
        let msg = format!("Readiness timeout: {e}");
        error!("[{}] {}", enc.name, msg);
        if !keep {
            let _ = teardown_enclave(&enc.name);
        }
        bail!(msg);
    }
    enc.ready_at = Some(Instant::now());
    info!(
        "[{}] Enclave ready after {:?}",
        enc.name,
        enc.ready_at
            .unwrap()
            .duration_since(enc.launched_at.unwrap())
    );

    // Phase 3: Observe
    info!("[{}] Observing for {min_epochs} epoch(s)...", enc.name);

    if let Err(e) = observe_enclave(&enc.name, min_epochs, target_epoch).await {
        let msg = format!("Observation failed: {e}");
        error!("[{}] {}", enc.name, msg);
        if !keep {
            let _ = teardown_enclave(&enc.name);
        }
        bail!(msg);
    }
    enc.observed_at = Some(Instant::now());

    // Phase 4: Run checks
    info!("[{}] Running checks...", enc.name);

    let check_result = run_checks(
        &enc.name,
        &enc.config,
        results_dir,
        strict,
        live_metrics,
        verbose,
        skip_finalization,
    )
    .await;

    enc.checked_at = Some(Instant::now());

    match check_result {
        Ok(summary) => {
            let result_str = summary.result.clone();
            enc.check_result = Some(summary);
            info!(
                "[{}] Checks complete: {} (total {:?})",
                enc.name,
                result_str,
                enc.checked_at.unwrap().duration_since(start)
            );
        }
        Err(e) => {
            let msg = format!("Check execution failed: {e}");
            error!("[{}] {}", enc.name, msg);
        }
    }

    // Phase 5: Tear down
    if !keep {
        info!("[{}] Tearing down enclave...", enc.name);
        if let Err(e) = teardown_enclave(&enc.name) {
            warn!("[{}] Teardown error (non-fatal): {e}", enc.name);
        }
    } else {
        info!("[{}] Keeping enclave running (--keep)", enc.name);
    }

    Ok(enc)
}

// ---------------------------------------------------------------------------
// Phase implementations
// ---------------------------------------------------------------------------

/// Phase 1: Launch a Kurtosis enclave.
async fn launch_enclave(name: &str, config: &Path, package: &str) -> Result<()> {
    // kurtosis resolves a RELATIVE package/args path against the engine context,
    // not the CLI cwd, and falls back to "no kurtosis.yml / Docker Compose" — so
    // pass ABSOLUTE paths (as run-and-verify.sh does). Fall back to the raw value
    // for a non-filesystem package ref (e.g. a github.com/... locator).
    let package_abs = std::fs::canonicalize(package)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| package.to_string());
    let config_abs = std::fs::canonicalize(config)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| config.display().to_string());
    let output = tokio::process::Command::new("kurtosis")
        .args([
            "run",
            &package_abs,
            "--enclave",
            name,
            "--args-file",
            &config_abs,
            "--image-download",
            "always",
        ])
        .output()
        .await
        .wrap_err("Failed to run kurtosis")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kurtosis run failed: {}", stderr.trim());
    }

    Ok(())
}

/// Phase 2: Wait for the enclave's beacon to reach the target epoch.
async fn wait_for_enclave_readiness(
    name: &str,
    target_epoch: u64,
    timeout_secs: u64,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_secs(10);

    // Discover the beacon URL
    let beacon_url = discover_beacon_url(name).await?;

    loop {
        if start.elapsed() >= timeout {
            bail!("Timeout waiting for enclave readiness after {timeout_secs}s");
        }

        // Query beacon head slot via the standard Beacon API
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let url = format!("{beacon_url}/eth/v1/beacon/headers/head");
        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await
                    && let Some(slot) = json
                        .get("data")
                        .and_then(|d| d.get("header"))
                        .and_then(|h| h.get("message"))
                        .and_then(|m| m.get("slot"))
                        .and_then(|s| s.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                {
                    let epoch = slot / 32;
                    if epoch >= target_epoch {
                        return Ok(());
                    }
                    tracing::debug!(
                        "[{}] Beacon at epoch {epoch}, waiting for {target_epoch}...",
                        name
                    );
                }
            }
            Err(e) => {
                tracing::debug!("[{}] Beacon not reachable yet: {e}", name);
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Phase 3: Observe the enclave for min_epochs.
///
/// Polls the beacon head slot and waits until min_epochs have passed since
/// the enclave became ready. Also does periodic health probes.
async fn observe_enclave(name: &str, min_epochs: u64, target_epoch: u64) -> Result<()> {
    let beacon_url = discover_beacon_url(name).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let start_slot = target_epoch * 32;
    let target_slot = start_slot + (min_epochs * 32);
    let poll_interval = Duration::from_secs(5);

    info!(
        "[{}] Observing: slot {start_slot} -> {target_slot} ({min_epochs} epochs)",
        name
    );

    let start = Instant::now();
    let timeout = Duration::from_secs(min_epochs * 32 * 12 + 120); // ~12s per slot + buffer

    loop {
        if start.elapsed() >= timeout {
            bail!("Observation timeout");
        }

        let url = format!("{beacon_url}/eth/v1/beacon/headers/head");
        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await
                    && let Some(slot) = json
                        .get("data")
                        .and_then(|d| d.get("header"))
                        .and_then(|h| h.get("message"))
                        .and_then(|m| m.get("slot"))
                        .and_then(|s| s.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                    && slot >= target_slot
                {
                    info!(
                        "[{}] Observation complete: slot {start_slot} -> {slot}",
                        name
                    );
                    return Ok(());
                }
            }
            Err(e) => {
                warn!("[{}] Health probe failed during observation: {e}", name);
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Phase 4: Run cb-verify against the enclave.
async fn run_checks(
    name: &str,
    config: &Path,
    results_dir: Option<&Path>,
    strict: bool,
    live_metrics: bool,
    verbose: bool,
    skip_finalization: bool,
) -> Result<CheckSummary> {
    // Find the cb-verify binary (built from the same crate)
    let manifest_path = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let binary_path = manifest_path.join("target/release/cb-verify");

    if !binary_path.exists() {
        bail!(
            "cb-verify binary not found at {}. Run 'cargo build --release' first.",
            binary_path.display()
        );
    }

    let mut cmd = tokio::process::Command::new(&binary_path);
    cmd.arg("--enclave").arg(name);
    cmd.arg("--config").arg(config);
    cmd.arg("--json");
    cmd.arg("--timeout").arg("3600");
    cmd.arg("--min-epochs").arg("0"); // Already observed
    cmd.arg("--target-epoch").arg("0"); // Already ready

    if let Some(dir) = results_dir {
        cmd.arg("--output-dir").arg(dir);
    }
    if strict {
        cmd.arg("--strict");
    }
    if live_metrics {
        cmd.arg("--live-metrics");
    }
    if verbose {
        cmd.arg("-v");
    }
    if skip_finalization {
        cmd.arg("--skip-finalization-check");
    }

    let output = cmd.output().await.wrap_err("Failed to run cb-verify")?;

    // Parse the JSON report from stdout
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find the JSON blob (cb-verify may print non-JSON lines before it)
    let json_start = stdout
        .find("{\n")
        .or_else(|| stdout.find("{\""))
        .unwrap_or(0);
    let json_str = &stdout[json_start..];

    let report: serde_json::Value =
        serde_json::from_str(json_str).wrap_err("Failed to parse cb-verify JSON output")?;

    let result = report
        .get("result")
        .and_then(|r| r.as_str())
        .unwrap_or("unknown")
        .to_string();

    let checks = report
        .get("checks")
        .and_then(|c| c.as_array())
        .map(|c| c.as_slice())
        .unwrap_or(&[]);

    let passed = checks
        .iter()
        .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("Pass"))
        .count();
    let failed = checks
        .iter()
        .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("Fail"))
        .count();
    let warnings = checks
        .iter()
        .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("Warn"))
        .count();
    let skipped = checks
        .iter()
        .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("Skip"))
        .count();

    Ok(CheckSummary {
        enclave: name.to_string(),
        config: config.display().to_string(),
        result,
        passed,
        failed,
        warnings,
        skipped,
        duration_secs: 0, // Will be filled in by caller
    })
}

/// Phase 5: Tear down an enclave.
fn teardown_enclave(name: &str) -> Result<()> {
    let output = Command::new("kurtosis")
        .args(["enclave", "rm", "-f", name])
        .output()
        .wrap_err("Failed to run kurtosis enclave rm")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kurtosis enclave rm failed: {}", stderr.trim());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Discover the beacon HTTP URL for an enclave by querying kurtosis port print.
async fn discover_beacon_url(enclave: &str) -> Result<String> {
    // Try common beacon service names
    let beacon_names = [
        "cl-1-lighthouse",
        "cl-1-prysm",
        "cl-1-teku",
        "cl-1-nimbus",
        "cl-1-lodestar",
    ];

    for name_prefix in &beacon_names {
        // Try to find the full service name
        let output = tokio::process::Command::new("kurtosis")
            .args(["enclave", "inspect", "--full-uuids", enclave])
            .output()
            .await
            .wrap_err("kurtosis enclave inspect failed")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let lower = line.to_lowercase();
            if lower.contains(name_prefix) && lower.contains("running") {
                // Found a running beacon service, get its HTTP port
                let service_name = line
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if service_name.is_empty() {
                    continue;
                }

                // Try to get the HTTP port URL
                let port_output = tokio::process::Command::new("kurtosis")
                    .args(["port", "print", enclave, &service_name, "http"])
                    .output()
                    .await;

                if let Ok(port_out) = port_output {
                    let url = String::from_utf8_lossy(&port_out.stdout).trim().to_string();
                    if !port_out.status.success() || url.is_empty() {
                        // Try "cl-http" as port name
                        let port_output2 = tokio::process::Command::new("kurtosis")
                            .args(["port", "print", enclave, &service_name, "cl-http"])
                            .output()
                            .await;
                        if let Ok(port_out2) = port_output2 {
                            let url2 = String::from_utf8_lossy(&port_out2.stdout)
                                .trim()
                                .to_string();
                            if !url2.is_empty() {
                                return Ok(url2);
                            }
                        }
                        continue;
                    }
                    return Ok(url);
                }
            }
        }
    }

    // Fallback: try to use the enclave's default beacon port
    bail!("Could not discover beacon URL for enclave '{enclave}'")
}

/// Derive an enclave name from a config filename.
fn enclave_name(config: &Path) -> String {
    let stem = config
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // Strip common prefixes like "cb-" for cleaner names
    let name = stem.strip_prefix("cb-").unwrap_or(stem);
    format!("CB-{name}")
}

/// Resolve config file paths: expand directories to *.yml files.
fn resolve_configs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut configs = Vec::new();

    if inputs.is_empty() {
        // Default: configs/generated/*.yml
        let default_dir = PathBuf::from("configs/generated");
        if default_dir.is_dir() {
            for entry in std::fs::read_dir(&default_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("yml")
                    || path.extension().and_then(|e| e.to_str()) == Some("yaml")
                {
                    configs.push(path);
                }
            }
        }
        configs.sort();
        return Ok(configs);
    }

    for input in inputs {
        if input.is_dir() {
            for entry in std::fs::read_dir(input)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("yml")
                    || path.extension().and_then(|e| e.to_str()) == Some("yaml")
                {
                    configs.push(path);
                }
            }
        } else if input.is_file() {
            configs.push(input.clone());
        } else {
            warn!("Skipping non-existent path: {}", input.display());
        }
    }

    configs.sort();
    Ok(configs)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_batch_summary(batch: &BatchReport) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                    Batch Verification Report                ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  Time:     {:48} ║", batch.timestamp);
    println!("║  Total:    {:48} ║", batch.total);
    println!("║  Passed:   {:48} ║", batch.passed.to_string().green());
    println!("║  Failed:   {:48} ║", batch.failed.to_string().red());
    println!("╠══════════════════════════════════════════════════════════════╣");

    for result in &batch.results {
        let status_icon = match result.result.as_str() {
            "PASS" => "✓".green(),
            "FAIL" => "✗".red(),
            _ => "?".yellow(),
        };
        let name = Path::new(&result.config)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&result.config);
        println!(
            "║  {} {:20}  {:6}  ({}p / {}f / {}w / {}s)  ║",
            status_icon,
            name,
            result.result,
            result.passed,
            result.failed,
            result.warnings,
            result.skipped
        );
    }

    println!("╚══════════════════════════════════════════════════════════════╝");
}
