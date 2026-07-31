//! `sim`: structured helix preflight + triage for Commit-Boost Kurtosis testnets.
//!
//! Task 0 scaffold. This bin reuses the shared library (`cb_testnet_verifier`)
//! rather than re-declaring modules. The subcommand bodies are stubs; the real
//! implementations land in later tasks (preflight = Task 3, triage = Task 2).
//!
//! Sync only: the verbs shell `kurtosis`/`docker` with `std::process::Command`,
//! matching `discovery.rs`. No tokio.

use std::path::Path;

use clap::Parser;

mod cli;
mod diagnose;
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
        Command::Generate { scenario, out_dir } => generate(scenario.as_deref(), &out_dir),
    }
}

/// Generate Kurtosis args-files (Task 1). Implemented in `generate::run`.
fn generate(scenario: Option<&str>, out_dir: &Path) {
    if let Err(e) = generate::run(scenario, out_dir) {
        tracing::error!(error = %e, "sim generate failed");
        eprintln!("generate error: {e:?}");
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
