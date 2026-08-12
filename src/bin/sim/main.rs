//! `sim`: structured helix preflight + triage for Commit-Boost Kurtosis testnets.
//!
//! Task 0 scaffold. This bin reuses the shared library (`cb_testnet_verifier`)
//! rather than re-declaring modules. The subcommand bodies are stubs; the real
//! implementations land in later tasks (preflight = Task 3, triage = Task 2).
//!
//! Sync only: the verbs shell `kurtosis`/`docker` with `std::process::Command`,
//! matching `discovery.rs`. No tokio.

use std::path::{Path, PathBuf};

use clap::Parser;
use eyre::WrapErr;

use genmodel::spec::ScenarioSpec;

mod checks_catalog;
mod cli;
mod diagnose;
mod diff;
mod doctor;
mod generate;
mod genmodel;
mod preflight;
mod render;
mod triage;

use cli::{Cli, Command, LogFormat};

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    match cli.command {
        Command::Preflight { args_file } => preflight(&args_file),
        Command::Triage { enclave } => triage(&enclave),
        Command::Checks { list, json } => checks(list, json),
        Command::Doctor => doctor(),
        Command::Diff { from, to, json } => diff_reports(&from, &to, json),
        Command::Generate {
            scenario,
            out_dir,
            check,
        } => generate(scenario.as_deref(), &out_dir, check),
        Command::Scenario {
            spec,
            base,
            set,
            out,
            show_spec,
        } => scenario_cmd(spec, base, set, out, show_spec),
    }
}

/// Render a composable scenario from a `ScenarioSpec` (`--spec <json>`) or a
/// named base with typed overrides (`--base`/`--set`). Implemented via
/// `ScenarioSpec::{from_json, from_base_and_overrides}` + `render`.
fn scenario_cmd(
    spec_path: Option<PathBuf>,
    base: Option<String>,
    set: Option<String>,
    out: Option<PathBuf>,
    show_spec: bool,
) {
    let result = (|| -> eyre::Result<()> {
        let spec = match &spec_path {
            Some(path) => {
                eyre::ensure!(
                    base.is_none() && set.is_none(),
                    "--spec is mutually exclusive with --base/--set"
                );
                let json = std::fs::read_to_string(path)
                    .wrap_err_with(|| format!("reading {}", path.display()))?;
                ScenarioSpec::from_json(&json)?
            }
            None => ScenarioSpec::from_base_and_overrides(base.as_deref(), set.as_deref())?,
        };
        if show_spec {
            eprintln!("{}", serde_json::to_string_pretty(&spec)?);
            let mut arms: Vec<String> =
                spec.armed_features().iter().map(|f| f.id().to_string()).collect();
            if spec.arms_min_bid() {
                arms.push("feature.min_bid".to_string());
            }
            if spec.arms_poison() {
                arms.push("poison_relay".to_string());
            }
            eprintln!("arms: [{}]", arms.join(", "));
        }
        let images = generate::images_from_env(Path::new(".env"));
        let rendered = spec.render(&spec.auto_comment(), &images, Path::new("keys"))?;
        match &out {
            Some(path) => {
                std::fs::write(path, &rendered)
                    .wrap_err_with(|| format!("writing {}", path.display()))?;
                println!("Rendered {}", path.display());
            }
            None => print!("{rendered}"),
        }
        Ok(())
    })();
    if let Err(e) = result {
        tracing::error!(error = %e, "sim scenario failed");
        eprintln!("scenario error: {e:?}");
        std::process::exit(1);
    }
}

/// Generate Kurtosis args-files (Task 1), or `--check` them (P2 drift gate).
/// Implemented in `generate::run` / `generate::check`.
fn generate(scenario: Option<&str>, out_dir: &Path, check: bool) {
    let result = if check {
        generate::check(scenario, out_dir)
    } else {
        generate::run(scenario, out_dir)
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "sim generate failed");
        eprintln!("generate error: {e:?}");
        std::process::exit(1);
    }
}

/// Emit the machine-readable check catalog (`sim checks --list [--json]`).
/// Implemented in `checks_catalog::run`.
fn checks(list: bool, json: bool) {
    if let Err(e) = checks_catalog::run(list, json) {
        tracing::error!(error = %e, "sim checks failed");
        eprintln!("checks error: {e:?}");
        std::process::exit(1);
    }
}

/// Compare two verification reports (`sim diff`). Implemented in `diff::run`;
/// exits nonzero if any check regressed (usable as a CI regression gate).
fn diff_reports(from: &Path, to: &Path, json: bool) {
    if let Err(e) = diff::run(from, to, json) {
        tracing::error!(error = %e, "sim diff failed");
        eprintln!("diff error: {e:?}");
        std::process::exit(1);
    }
}

/// Host-prerequisite preflight for a devnet (`sim doctor`). Implemented in
/// `doctor::run`; exits nonzero if a hard prerequisite (kurtosis/docker) is missing.
fn doctor() {
    if let Err(e) = doctor::run() {
        tracing::error!(error = %e, "sim doctor failed");
        eprintln!("doctor error: {e:?}");
        std::process::exit(1);
    }
}

/// Initialize the `tracing` subscriber. `--log-format json` emits one JSON
/// object per event; otherwise a pretty human rendering.
fn init_tracing(format: LogFormat) {
    match format {
        LogFormat::Json => tracing_subscriber::fmt().json().init(),
        LogFormat::Pretty => tracing_subscriber::fmt().init(),
    }
}

/// Validate a launch args-file's helix config against the real image before a
/// run (Task 3). Implemented in `preflight::run`; exits nonzero on a `Fail`.
fn preflight(args_file: &Path) {
    if let Err(e) = preflight::run(args_file) {
        tracing::error!(args_file = %args_file.display(), error = %e, "sim preflight failed");
        eprintln!("preflight error: {e:?}");
        std::process::exit(1);
    }
}

/// Attach to an already-broken enclave and extract each dead service's root
/// cause (Task 2). Implemented in `triage::run`.
fn triage(enclave: &str) {
    if let Err(e) = triage::run(enclave) {
        tracing::error!(enclave, error = %e, "sim triage failed");
        eprintln!("triage error: {e:?}");
        std::process::exit(1);
    }
}
