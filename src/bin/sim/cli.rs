//! `sim` CLI surface (clap derive).
//!
//! Task 0 scaffold: the argument shape is wired; the subcommand bodies are
//! stubs implemented in later tasks (preflight = Task 3, triage = Task 2).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Structured preflight + triage for Commit-Boost Kurtosis testnets.
#[derive(Debug, Parser)]
#[command(name = "sim", about = "helix preflight + triage for the sim harness")]
pub struct Cli {
    /// Output format for the structured `tracing` stream.
    #[arg(long, value_enum, global = true, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,

    #[command(subcommand)]
    pub command: Command,
}

/// How the structured event stream is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Human-readable rendering (default).
    Pretty,
    /// One JSON object per event (for agents / machine consumption).
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate a launch args-file BEFORE running the testnet (helix config-parse).
    Preflight {
        /// Path to the kurtosis args-file to validate.
        args_file: PathBuf,
    },
    /// Attach to an already-broken enclave and extract each service's root cause.
    Triage {
        /// Name of the kurtosis enclave to triage.
        enclave: String,
    },
    /// Generate Kurtosis args-files for the CB test scenarios (Rust port of the
    /// retired `generate_kurtosis_configs.py`).
    Generate {
        /// Scenario name (e.g. `cb-basic`); omit to generate all six.
        scenario: Option<String>,
        /// Directory to write the generated `<scenario>.yml` files into.
        #[arg(long, default_value = "configs/generated")]
        out_dir: PathBuf,
        /// Don't write; instead verify the on-disk configs already match what the
        /// generator would produce, and exit nonzero on any drift (CI / agent gate).
        #[arg(long)]
        check: bool,
    },
}
