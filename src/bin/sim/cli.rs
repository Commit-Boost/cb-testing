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
    /// Emit the machine-readable catalog of what `cb-verify` asserts, so an agent
    /// can discover the harness's contract without reading source or CHECKS.md.
    Checks {
        /// Emit the check catalog (required — without it there is nothing to do).
        #[arg(long)]
        list: bool,
        /// Emit the catalog as JSON instead of a readable table.
        #[arg(long)]
        json: bool,
    },
    /// Host-prerequisite preflight for a devnet: kurtosis, docker, memory
    /// headroom, the CB image, and the ethereum-package submodule.
    Doctor,
    /// Compare two verification reports (JSON) and surface the verdict delta.
    /// Exits nonzero if any check regressed — usable as a CI regression gate
    /// after an image bump.
    Diff {
        /// The baseline report (the "from" side).
        from: PathBuf,
        /// The new report (the "to" side).
        to: PathBuf,
        /// Emit the structured diff as JSON instead of a readable summary.
        #[arg(long)]
        json: bool,
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
        /// Also emit the curated composable coverage configs (the additional CL
        /// clients + high-value feature combos; rendered from `ScenarioSpec`).
        #[arg(long)]
        curated: bool,
    },
    /// Render a COMPOSABLE scenario config from a structured `ScenarioSpec` — a
    /// full JSON spec, or a named base with typed field overrides. Unlike
    /// `generate` (the 13 frozen named scenarios), this composes features freely
    /// (e.g. ws + prysm + timing-games). Output is a Kurtosis args-file, valid by
    /// construction (it renders through the same seams the goldens pin).
    Scenario {
        /// Path to a `ScenarioSpec` JSON file (the full structured surface).
        /// Mutually exclusive with `--base`/`--set`.
        #[arg(long)]
        spec: Option<PathBuf>,
        /// A named scenario to start from (e.g. `cb-mux`); default `cb-basic`.
        #[arg(long)]
        base: Option<String>,
        /// Comma-separated `key=value` field overrides applied onto the base,
        /// e.g. `--set get_header=stream,clients=nethermind-prysm,timing_games=true`.
        #[arg(long)]
        set: Option<String>,
        /// Write the rendered args-file here (default: stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the resolved spec as JSON to stderr before rendering (preview).
        #[arg(long)]
        show_spec: bool,
    },
}
