//! cb-verify: Automated verification for Commit-Boost Kurtosis testnets.
//!
//! Discovers services in a running enclave, polls for readiness,
//! runs verification checks, and produces a structured report.

#![allow(unused_imports)]
#![allow(dead_code)]

use std::time::{Duration, Instant};

use clap::Parser;
use eyre::Result;
use tracing::{debug, error, info, warn};

mod beacon;
mod checks;
mod discovery;
mod health;
mod live;
mod metrics;
mod relay;
mod report;

use beacon::BeaconClient;
use checks::{CheckResult, CheckStatus};
use health::{HealthTarget, ServiceKind};
use live::{LIVE_METRICS_FILTER, compute_deltas, format_delta_json, format_delta_log};
use relay::RelayClient;
use report::{ObservationWindow, VerificationReport};

const SLOTS_PER_EPOCH: u64 = 32;

/// Verify Commit-Boost MEV pipeline in a Kurtosis devnet.
///
/// Two modes of operation:
///
/// 1. Attached: Run alongside a testnet launched by `run-and-verify.sh`.
///    The enclave is specified with --enclave and the verifier waits for
///    readiness, observes, and checks.
///
/// 2. Standalone: Point the verifier at a running enclave with --enclave.
///    It checks whatever data is available and reports whether the pipeline
///    is healthy. Use --config to also verify mux routing rules.
///
/// Examples:
///   # Attached mode (launches testnet + verifies)
///   ./scripts/run-and-verify.sh --config configs/cb-mux.yml
///
///   # Standalone: quick health check (no observation window)
///   cb-verify --enclave CB-Testnet --min-epochs 0
///
///   # Standalone: full verification with mux checks
///   cb-verify --enclave CB-Testnet --config configs/cb-mux.yml
///
///   # Standalone: show raw CB PBS logs for debugging
///   cb-verify --enclave CB-Testnet --show-logs
///
///   # Standalone, just check current health (no observation window)
///   cb-verify --enclave CB-Testnet --min-epochs 0
#[derive(Parser, Debug)]
#[command(name = "cb-verify", version, about)]
struct Cli {
    /// Kurtosis enclave name.
    #[arg(long)]
    enclave: Option<String>,

    /// Path to the Kurtosis config file (YAML).
    ///
    /// Used to extract the embedded Commit-Boost config for mux
    /// verification. If no [[mux]] sections are found, the mux check
    /// is skipped.
    ///
    /// The enclave name must be provided separately via --enclave.
    #[arg(long)]
    config: Option<String>,

    /// Observation window width in epochs. Combined with --target-epoch:
    /// window = [target_epoch, target_epoch + min_epochs).
    /// Set to 0 to skip observation and run checks against current slot.
    #[arg(long, default_value_t = 2)]
    min_epochs: u64,

    /// Epoch at which the observation window starts.
    ///
    /// Combined with --min-epochs to define the slot range for checks.
    /// e.g. --target-epoch 2 --min-epochs 1 → observe slots 64-96 (epoch 2).
    /// The verifier waits for the chain to reach the end slot, then queries
    /// historical relay/beacon data. No real-time slot-watching loop.
    ///
    /// (default 7: genesis + validator activation + relay registration + builder warm-up)
    #[arg(long, default_value_t = 7)]
    target_epoch: u64,

    /// Max seconds to wait for devnet readiness.
    #[arg(long, default_value_t = 3600)]
    timeout: u64,

    /// Minimum MEV delivery rate threshold.
    #[arg(long, default_value_t = 0.30)]
    mev_threshold: f64,

    /// Output JSON report instead of terminal colors.
    #[arg(long)]
    json: bool,

    /// Enable debug logging.
    #[arg(short, long)]
    verbose: bool,

    /// Strict mode: promote soft warnings to FAIL. Affects:
    ///
    /// - get_header with zero 200s but some 204s (relay alive but no bids
    ///   ever delivered -- builder idle or below threshold).
    /// - submit_blinded_block with zero (200+202) deliveries (proposer
    ///   never chose a builder block in the observation window).
    ///
    /// 5xx responses always FAIL regardless of this flag. Use in CI.
    #[arg(long)]
    strict: bool,

    /// Enable live metrics polling during observation window. Scrapes :9090/metrics
    /// every 30s and logs counter deltas as they occur. Skips if metrics URL not
    /// directly accessible.
    #[arg(long)]
    live_metrics: bool,

    /// Print raw CB PBS service logs to stdout for debugging.
    ///
    /// Fetches the last N log lines from each CB PBS service and prints them
    /// in a human-readable format. Does not run any verification checks.
    #[arg(long)]
    show_logs: bool,

    /// Skip finalization check entirely.
    ///
    /// By default, chain finality (finalized epoch >= 2) is checked only when
    /// the observation window ends at epoch 3+ (slot 96), where finalization
    /// is expected to have occurred. Use this flag to force-skip even then.
    #[arg(long)]
    skip_finalization_check: bool,

    /// Directory to save JSON report files. Requires --json.
    ///
    /// When set, writes `{enclave}.json` into this directory after the report
    /// is printed to stdout. Useful for batch runs (e.g., `just test-all`)
    /// where each config variant produces its own JSON file.
    ///
    /// The directory must exist before the run starts.
    #[arg(long)]
    output_dir: Option<String>,
}

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

    let code = run_verification(&cli).await;
    std::process::exit(code);
}

/// Resolve the enclave name and CB config from CLI args.
/// The enclave name is required. The config is optional and used for
/// mux verification.
fn resolve_enclave_and_config(cli: &Cli) -> Result<(String, Option<String>)> {
    let enclave = cli.enclave.clone().ok_or_else(|| {
        eyre::eyre!("Must provide --enclave to specify a running enclave")
    })?;
    Ok((enclave, cli.config.clone()))
}

async fn run_verification(cli: &Cli) -> i32 {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Helper: save JSON report to file if --output-dir was set.
    let save_report = |report: &VerificationReport| {
        if let Some(ref dir) = cli.output_dir {
            report::save_json_report(report, dir);
        }
    };

    // Step 0: Resolve enclave name and CB config
    let (enclave_name, cb_config) = match resolve_enclave_and_config(cli) {
        Ok(result) => result,
        Err(e) => {
            error!("{e}");
            let report = make_error_report("unknown", &now, &format!("{e}"));
            report::print_report(&report, cli.json);
            save_report(&report);
            return 2;
        }
    };

    // Step 1: Discover services
    info!("Discovering services in enclave '{}'...", enclave_name);
    let services = match discovery::discover(&enclave_name) {
        Ok(s) => s,
        Err(e) => {
            error!("Service discovery failed: {e}");
            let report = make_error_report(&enclave_name, &now, &format!("Discovery failed: {e}"));
            report::print_report(&report, cli.json);
            save_report(&report);
            return 2;
        }
    };

    if services.beacon_urls.is_empty() {
        error!("No beacon nodes found in enclave");
        let report = make_error_report(&enclave_name, &now, "No beacon nodes found");
        report::print_report(&report, cli.json);
        save_report(&report);
        return 2;
    }

    let beacon = BeaconClient::new(&services.beacon_urls[0]);
    let relays: Vec<RelayClient> = services.relay_urls.iter().map(RelayClient::new).collect();
    let metrics_url = services.cb_metrics_urls.first().map(|s| s.as_str());

    info!("Beacon: {}", services.beacon_urls[0]);
    info!("Relays: {:?}", services.relay_urls);
    info!("CB metrics: {}", metrics_url.unwrap_or("not available"));

    // --show-logs mode: print raw CB PBS logs and exit
    if cli.show_logs {
        return show_cb_logs(&enclave_name, &services.cb_service_names, &now, cli.json, &save_report);
    }

    if relays.is_empty() {
        warn!("No relay URLs found -- relay checks will fail");
    }

    // Step 2: Compute observation window from target_epoch + min_epochs.
    //
    // Observation is anchored to the target epoch, not to the current slot.
    // e.g. --target-epoch 2 --min-epochs 1 → observe slots 64-96 (epoch 2).
    // This means checks query historical relay/beacon data for those slots,
    // and we only need the chain to have reached the end of the window.
    let (start_slot, end_slot) = if cli.min_epochs > 0 {
        let s = cli.target_epoch * SLOTS_PER_EPOCH;
        let e = (cli.target_epoch + cli.min_epochs) * SLOTS_PER_EPOCH;
        info!(
            "Observation window: epoch {} → {} (slots {s} → {e})",
            cli.target_epoch,
            cli.target_epoch + cli.min_epochs,
        );
        (s, e)
    } else {
        // min_epochs=0: use current slot as both start and end
        let slot = match beacon.get_head_slot().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to get current slot: {e}");
                let report = make_error_report(&enclave_name, &now, &format!("Failed to get current slot: {e}"));
                report::print_report(&report, cli.json);
                save_report(&report);
                return 2;
            }
        };
        info!("Skipping observation window (min_epochs=0), using current slot {slot}");
        (slot, slot)
    };

    // Build the health-monitoring set -- anything whose disappearance would
    // invalidate the run. We track every beacon node, every relay, and every
    // CB PBS endpoint. Dead = run aborts.
    let health_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut targets: Vec<HealthTarget> = Vec::new();
    for (i, url) in services.beacon_urls.iter().enumerate() {
        targets.push(HealthTarget::new(
            format!("beacon[{i}]"),
            url,
            ServiceKind::Beacon,
        ));
    }
    for (i, url) in services.relay_urls.iter().enumerate() {
        targets.push(HealthTarget::new(
            format!("relay[{i}]"),
            url,
            ServiceKind::Relay,
        ));
    }
    for (i, url) in services.cb_pbs_urls.iter().enumerate() {
        targets.push(HealthTarget::new(
            format!("cb-pbs[{i}]"),
            url,
            ServiceKind::CbPbs,
        ));
    }

    // Step 2b: Tier 0 connectivity preflight.
    //
    // Fail loud, fail once. Probe every service we care about; if anything
    // that *should* be reachable isn't, bail before the wait instead of
    // emitting confusing errors for each downstream check.
    info!("Running preflight on {} target(s)...", targets.len());
    let dead_at_preflight = health::probe_all(&health_client, &targets).await;
    if !dead_at_preflight.is_empty() {
        for (label, e) in &dead_at_preflight {
            warn!("  {label} UNREACHABLE: {e}");
        }

        let summary: Vec<String> = dead_at_preflight.iter().map(|(l, _)| l.clone()).collect();

        // When ONLY relay targets are dead, try post-mortem: query the relay's
        // Postgres directly. If the pipeline worked before the crash, Postgres
        // still has the evidence. Salvage the verdict instead of hard-failing.
        let all_are_relays = dead_at_preflight
            .iter()
            .all(|(l, _)| l.starts_with("relay["));
        let relay_died = dead_at_preflight
            .iter()
            .any(|(l, _)| l.starts_with("relay["));

        if all_are_relays && relay_died {
            info!("Relay Data API unreachable — attempting post-mortem via Postgres...");
            let postmortem = discovery::query_mev_relay_postgres(&enclave_name);
            if !postmortem.is_empty() {
                info!(
                    "Post-mortem: found {} payload(s) in relay Postgres before crash:",
                    postmortem.len()
                );
                for r in &postmortem {
                    let hash_short = if r.block_hash.len() > 28 {
                        &r.block_hash[..28]
                    } else {
                        &r.block_hash
                    };
                    info!("  slot={} hash={}... value={}", r.slot, hash_short, r.value);
                }
                info!(
                    "Pipeline worked before relay crash. Proceeding with non-relay checks \
                     (relay API checks will SKIP)."
                );
                // Fall through to Step 3 — wait for window. Relay checks
                // will naturally SKIP because the relay URLs are unreachable.
            } else {
                error!("Post-mortem: no delivery records found in relay Postgres.");
                let report = make_error_report(
                    &enclave_name,
                    &now,
                    &format!(
                        "Preflight failed ({} of {} services): {}. Relay API unreachable \
                         and post-mortem Postgres query found no delivery records. \
                         Try: kurtosis enclave inspect {} ; docker ps -a",
                        dead_at_preflight.len(),
                        targets.len(),
                        summary.join(", "),
                        &enclave_name
                    ),
                );
                report::print_report(&report, cli.json);
                save_report(&report);
                return 2;
            }
        } else {
            error!(
                "{} of {} service(s) unreachable: {:?}",
                dead_at_preflight.len(),
                targets.len(),
                summary
            );
            let report = make_error_report(
                &enclave_name,
                &now,
                &format!(
                    "Preflight failed ({} of {} services): {}. Kurtosis port mapping may be \
                     stale or a container crashed. Try: kurtosis enclave inspect {} ; docker ps -a",
                    dead_at_preflight.len(),
                    targets.len(),
                    summary.join(", "),
                    &enclave_name
                ),
            );
            report::print_report(&report, cli.json);
            save_report(&report);
            return 2;
        }
    }
    info!("  All {} service(s) reachable", targets.len());

    // Step 3: Wait for chain to reach end of observation window.
    //
    // No real-time slot-watching loop — the window is pre-computed from
    // target_epoch + min_epochs. We just wait until the head passes end_slot,
    // then query historical relay/beacon data for those slots. Live metrics
    // are polled during the wait if --live-metrics is set.
    //
    // When min_epochs is 0, skip the wait entirely.
    if !wait_for_slot(
        &beacon,
        &health_client,
        &targets,
        end_slot,
        cli.timeout,
        WaitLiveOpts {
            metrics_url,
            live_metrics: cli.live_metrics,
            json_output: cli.json,
        },
    )
    .await
    {
        let report = make_error_report(
            &enclave_name,
            &now,
            &format!("Chain did not reach slot {end_slot} within {}s", cli.timeout),
        );
        report::print_report(&report, cli.json);
        save_report(&report);
        return 2;
    }

    let window = ObservationWindow {
        start_slot,
        end_slot,
    };

    // Step 4: Run all checks
    let mut all_checks: Vec<CheckResult> = Vec::new();

    info!("Running chain health checks...");
    all_checks.extend(
        checks::chain_health::run_chain_health_checks(
            &beacon,
            window.start_slot,
            window.end_slot,
            &enclave_name,
            cli.skip_finalization_check,
        )
        .await,
    );

    // Fetch validator pubkeys for tier-3 relay registration check. SKIP the
    // tier-3 check if this fails rather than aborting the whole run.
    let validator_pubkeys: Vec<String> = match beacon.get_active_validator_pubkeys().await {
        Ok(pks) => {
            info!("Fetched {} active validator pubkey(s)", pks.len());
            pks
        }
        Err(e) => {
            warn!("Failed to fetch validator pubkeys (registration check will SKIP): {e}");
            Vec::new()
        }
    };

    info!("Running relay pipeline checks...");
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    all_checks.extend(
        checks::relay_pipeline::run_relay_checks(
            &relays,
            &beacon,
            window.start_slot,
            window.end_slot,
            cli.mev_threshold,
            &validator_pubkeys,
            &http_client,
        )
        .await,
    );

    info!("Running payload matching checks...");
    all_checks.extend(
        checks::payload_matching::run_payload_checks(
            &relays,
            &beacon,
            window.start_slot,
            window.end_slot,
        )
        .await,
    );

    info!("Running CB metrics checks...");
    all_checks.extend(
        checks::cb_metrics::run_metrics_checks(
            &http_client,
            metrics_url,
            Some(enclave_name.as_str()),
            &services.cb_service_names,
            cli.strict,
        )
        .await,
    );

    // MUX routing check (optional — requires config with [[mux]] sections)
    if let Some(ref cb_path) = cb_config {
        info!("Checking for [[mux]] sections in CB config: {cb_path}...");
        match checks::mux_routing::extract_mux_from_config(cb_path) {
            Ok(Some(mux_entries)) => {
                info!(
                    "Found {} [[mux]] section(s) — running MUX routing verification",
                    mux_entries.len()
                );
                all_checks.push(
                    checks::mux_routing::check_mux_routing(
                        &enclave_name,
                        &services.cb_service_names,
                        &mux_entries,
                    )
                    .await,
                );
            }
            Ok(None) => {
                info!("No [[mux]] sections found in CB config — skipping MUX check");
            }
            Err(e) => {
                all_checks.push(CheckResult::fail(
                    "mux.routing",
                    1,
                    format!("Failed to parse CB config '{cb_path}': {e}"),
                ));
            }
        }
    } else {
        info!("No --cb-config provided — skipping MUX routing check");
    }

    // Step 5: Report
    let tier1_failed = all_checks
        .iter()
        .any(|c| c.tier == 1 && c.status == CheckStatus::Fail);

    let report = VerificationReport {
        enclave: enclave_name.clone(),
        timestamp: now,
        observation_window: Some(window),
        result: if tier1_failed {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        checks: all_checks,
    };

    report::print_report(&report, cli.json);
    save_report(&report);
    report::exit_code(&report)
}

/// Poll the beacon node until the devnet is ready for verification.
/// Live-metrics options for the wait phase.
struct WaitLiveOpts<'a> {
    metrics_url: Option<&'a str>,
    live_metrics: bool,
    json_output: bool,
}

/// Wait for the beacon chain head to reach `target_slot`.
///
/// No finalization gate — just wait for head advancement. Interleaves
/// health probes every ~30s (fail fast if a service dies) and live metrics
/// scraping if requested. Returns false on timeout.
async fn wait_for_slot(
    beacon: &BeaconClient,
    http: &reqwest::Client,
    targets: &[HealthTarget],
    target_slot: u64,
    timeout: u64,
    live_opts: WaitLiveOpts<'_>,
) -> bool {
    info!("Waiting for chain to reach slot {target_slot} (verification starts there, timeout {timeout}s)...");

    let start = Instant::now();
    let timeout_dur = Duration::from_secs(timeout);
    let poll_interval = Duration::from_secs(5);
    // Probe services every N poll ticks -- 6 * 5s = 30s.
    const PROBE_EVERY_N_TICKS: u32 = 6;
    let mut tick: u32 = 0;

    // Live metrics setup: initial scrape + state
    let prev_scrape: Option<prometheus_parse::Scrape> = if live_opts.live_metrics {
        match live_opts.metrics_url {
            Some(url) => match metrics::fetch_metrics(http, url).await {
                Ok(s) => {
                    info!("live: initial scrape captured, polling every 30s");
                    Some(s)
                }
                Err(e) => {
                    warn!("live: initial metrics scrape failed: {e}");
                    None
                }
            },
            None => {
                warn!(
                    "--live-metrics requested but metrics not HTTP-reachable; skipping live deltas"
                );
                None
            }
        }
    } else {
        None
    };

    loop {
        if start.elapsed() >= timeout_dur {
            error!("Timeout: chain did not reach slot {target_slot} within {timeout}s");
            return false;
        }

        // Check syncing
        match beacon.is_syncing().await {
            Err(_) => {
                info!("  Beacon node not reachable yet...");
                tokio::time::sleep(poll_interval).await;
                continue;
            }
            Ok(true) => {
                info!("  Beacon node still syncing...");
                tokio::time::sleep(poll_interval).await;
                continue;
            }
            Ok(false) => {}
        }

        let head = beacon.get_head_slot().await.ok();
        let finalized = beacon.get_finalized_epoch().await.ok();
        let current_epoch = head.map(|h| h / SLOTS_PER_EPOCH).unwrap_or(0);

        info!(
            "  head_slot={:?} epoch={current_epoch} finalized_epoch={:?} (waiting for slot {target_slot} to begin verification)",
            head, finalized
        );

        if let Some(h) = head
            && h >= target_slot
        {
            info!("Chain reached slot {h} >= {target_slot}. Starting verification...");
            return true;
        }

        // Periodic service liveness check. A stopped container doesn't refuse
        // politely -- it TCP-resets or times out, both of which surface here.
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(PROBE_EVERY_N_TICKS) {
            let dead = health::probe_all(http, targets).await;
            if let Some((label, err)) = dead.into_iter().next() {
                warn!("  {label} health probe failed mid-wait: {err}");
                // Don't abort — single probe failure could be transient.
                // If it stays dead, downstream checks will catch it.
            }

            // Live metrics: scrape, compute deltas vs previous, log.
            if live_opts.live_metrics
                && let Some(url) = live_opts.metrics_url
            {
                match metrics::fetch_metrics(http, url).await {
                    Ok(curr) => {
                        let deltas =
                            compute_deltas(prev_scrape.as_ref(), &curr, LIVE_METRICS_FILTER);
                        if !deltas.is_empty() {
                            if live_opts.json_output {
                                for line in format_delta_json(&deltas) {
                                    eprintln!("{line}");
                                }
                            } else {
                                for line in format_delta_log(&deltas) {
                                    info!("{line}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!("live: metrics scrape failed (non-fatal): {e}");
                    }
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Fetch and print raw CB PBS service logs for debugging.
fn show_cb_logs(
    enclave_name: &str,
    cb_service_names: &[String],
    now: &str,
    json_mode: bool,
    save_report: &dyn Fn(&VerificationReport),
) -> i32 {
    use crate::checks::mux_routing::{parse_cb_log_line, fetch_service_logs};

    println!("\n=== CB PBS Service Logs ===");
    println!("Enclave: {enclave_name}");
    println!("Services: {}\n", cb_service_names.join(", "));

    let mut total_events = 0;
    let mut parsed_events = 0;

    for service_name in cb_service_names {
        println!("--- {service_name} ---");
        match fetch_service_logs(enclave_name, service_name) {
            Ok(logs) => {
                if logs.is_empty() {
                    println!("  (no relevant log lines)");
                    continue;
                }
                for line in logs.lines() {
                    total_events += 1;
                    if let Some(event) = parse_cb_log_line(line) {
                        parsed_events += 1;
                        print!("  [{}] {}", event.message, event.slot.map(|s| format!("slot={}", s)).unwrap_or_default());
                        if let Some(ref mux) = event.mux_id {
                            print!(" mux={}", mux);
                        }
                        if let Some(ref relay) = event.relay_id {
                            print!(" relay={}", relay);
                        }
                        if let Some(ref val) = event.validator {
                            let short = if val.len() > 20 { &val[..20] } else { val };
                            print!(" val={}...", short);
                        }
                        println!();
                    } else {
                        // Print raw line if parsing failed
                        let short = if line.len() > 120 { &line[..120] } else { line };
                        println!("  [RAW] {}...", short);
                    }
                }
            }
            Err(e) => {
                println!("  ERROR: {e}");
            }
        }
    }

    println!("\nTotal: {} log lines, {} parsed successfully", total_events, parsed_events);

    let report = VerificationReport {
        enclave: enclave_name.to_string(),
        timestamp: now.to_string(),
        observation_window: None,
        result: CheckStatus::Pass,
        checks: vec![CheckResult::pass(
            "logs",
            1,
            format!("Fetched {} log lines from {} service(s)", total_events, cb_service_names.len()),
        )],
    };
    report::print_report(&report, json_mode);
    save_report(&report);
    0
}

fn make_error_report(enclave: &str, timestamp: &str, detail: &str) -> VerificationReport {
    VerificationReport {
        enclave: enclave.to_string(),
        timestamp: timestamp.to_string(),
        observation_window: None,
        result: CheckStatus::Fail,
        checks: vec![CheckResult::fail("setup", 1, detail)],
    }
}
