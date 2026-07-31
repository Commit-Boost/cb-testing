//! `sim triage <enclave>` — attach to a broken enclave, extract each dead
//! service's ROOT cause, emit a structured JSON report (Task 2, the wiring).
//!
//! Design (J): observability is a property of the run. The JSON `TriageReport`
//! is the single source of truth — a human reads a pretty rendering, an agent
//! reads the JSON, both off the same surface. `triage` is just one entry point.
//!
//! The masking fix: `kurtosis service logs` routes through a broker that can
//! surface a grpc UTF-8 error instead of the real panic, or return empty on a
//! fast-exit race / when a service-add aborted (leaving the service
//! UNREGISTERED). So we try `kurtosis service logs` first and FALL BACK to
//! `docker logs` on the resolved container whenever that is empty / errors /
//! yields no root cause.
//!
//! Only this module does process I/O; `diagnose::extract_root_cause` stays pure.
//! The `run` path needs kurtosis + docker, so it is a manual/Docker-gated smoke
//! check, NOT a `cargo test` (only the pure `parse_service_statuses` /
//! `services_to_triage` cores are unit-tested).

use std::process::Command;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use serde::Serialize;

use cb_testnet_verifier::discovery::split_on_multi_space;

use crate::diagnose::{RootCause, extract_root_cause};

/// Wall-clock bound for every shell we run (kurtosis/docker can hang).
const SHELL_TIMEOUT_SECS: u64 = 30;

/// One service row from `kurtosis enclave inspect`, keeping the STATUS column
/// that `discovery::parse_services` discards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub status: String,
}

/// A crashed service plus its extracted root cause.
#[derive(Debug, Clone, Serialize)]
pub struct FailedService {
    pub service: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<RootCause>,
}

/// The structured triage output (the source of truth for human + agent).
#[derive(Debug, Clone, Serialize)]
pub struct TriageReport {
    pub enclave: String,
    pub failed: Vec<FailedService>,
}

/// Parse the STATUS column out of `kurtosis enclave inspect` text.
///
/// Reuses `discovery::split_on_multi_space` for the exact column split used to
/// read the `User Services` table. The status is the LAST column of each row
/// (`UUID  Name  Ports…  Status`); ports may themselves span multiple columns,
/// so we key off the row's last field rather than a fixed index.
pub fn parse_service_statuses(inspect_output: &str) -> Vec<ServiceStatus> {
    let mut out = Vec::new();
    let mut in_services = false;
    let mut header_seen = false;

    for line in inspect_output.lines() {
        let stripped = line.trim();

        if stripped.contains("User Services") {
            in_services = true;
            header_seen = false;
            continue;
        }
        if !in_services {
            continue;
        }
        // A new top-level section ends the User Services block.
        if header_seen && stripped.contains("====") && !stripped.contains("User Services") {
            break;
        }
        if stripped.starts_with("====") || stripped.starts_with("----") || stripped.is_empty() {
            continue;
        }
        if stripped.contains("UUID") && stripped.contains("Name") {
            header_seen = true;
            continue;
        }
        if !header_seen {
            continue;
        }

        let parts = split_on_multi_space(stripped);
        // Need at least UUID, Name, Status.
        if parts.len() < 3 {
            continue;
        }
        let name = parts[1].trim().to_string();
        let status = parts[parts.len() - 1].trim().to_string();
        if name.is_empty() || status.is_empty() {
            continue;
        }
        out.push(ServiceStatus { name, status });
    }

    out
}

/// Decide which services need triage: every non-RUNNING service from inspect,
/// UNIONed with any `known_crashed` name that inspect never listed (the
/// half-built-enclave case, where a service-add aborted before registration).
///
/// Pure so it can be unit-tested; `run` supplies the process-derived inputs.
pub fn services_to_triage(
    statuses: &[ServiceStatus],
    known_crashed: &[&str],
) -> Vec<ServiceStatus> {
    let mut result: Vec<ServiceStatus> = statuses
        .iter()
        .filter(|s| !s.status.eq_ignore_ascii_case("RUNNING"))
        .cloned()
        .collect();

    for &name in known_crashed {
        let listed = statuses.iter().any(|s| s.name == name);
        if !listed {
            result.push(ServiceStatus {
                name: name.to_string(),
                status: "UNREGISTERED".to_string(),
            });
        }
    }

    result
}

/// Entry point for `sim triage <enclave>`.
pub fn run(enclave: &str) -> Result<()> {
    let report = triage(enclave, &[])?;
    let json = serde_json::to_string_pretty(&report).wrap_err("serialize triage report")?;
    println!("{json}");
    Ok(())
}

/// Build the triage report for an enclave, optionally forcing triage of
/// `known_crashed` services that inspect may not list (half-built enclave).
fn triage(enclave: &str, known_crashed: &[&str]) -> Result<TriageReport> {
    let inspect = run_inspect(enclave)?;
    let statuses = parse_service_statuses(&inspect);
    let targets = services_to_triage(&statuses, known_crashed);

    let mut failed = Vec::new();
    for target in targets {
        tracing::info!(service = %target.name, status = %target.status, "triaging service");
        let logs = collect_logs(enclave, &target.name, &inspect);
        let root_cause = logs.as_deref().and_then(extract_root_cause);
        failed.push(FailedService {
            service: target.name,
            status: target.status,
            root_cause,
        });
    }

    Ok(TriageReport {
        enclave: enclave.to_string(),
        failed,
    })
}

/// `kurtosis enclave inspect --full-uuids <enclave>` (stdout, best effort).
fn run_inspect(enclave: &str) -> Result<String> {
    let out = sh_capture("kurtosis", &["enclave", "inspect", "--full-uuids", enclave])?;
    Ok(out.stdout)
}

/// Get a service's logs with the masking fix: try `kurtosis service logs`; if it
/// errors / is empty / carries no extractable root cause, fall back to
/// `docker logs` on the resolved container.
fn collect_logs(enclave: &str, service: &str, inspect: &str) -> Option<String> {
    if let Ok(out) = sh_capture("kurtosis", &["service", "logs", enclave, service]) {
        let combined = out.combined();
        if !combined.trim().is_empty() && extract_root_cause(&combined).is_some() {
            return Some(combined);
        }
    }

    // Masking / empty / race: go straight to the container.
    if let Some(container) = resolve_container(service, inspect)
        && let Ok(out) = sh_capture("docker", &["logs", &container])
    {
        let combined = out.combined();
        if !combined.trim().is_empty() {
            return Some(combined);
        }
    }

    None
}

/// Resolve a docker container name for a kurtosis service. Best effort: kurtosis
/// container names embed the service name, so we match it against `docker ps -a`.
fn resolve_container(service: &str, _inspect: &str) -> Option<String> {
    let out = sh_capture("docker", &["ps", "-a", "--format", "{{.Names}}"]).ok()?;
    out.stdout
        .lines()
        .map(str::trim)
        .find(|name| name.contains(service))
        .map(str::to_string)
}

/// Captured output of a bounded shell.
struct ShellOutput {
    stdout: String,
    stderr: String,
}

impl ShellOutput {
    /// stdout + stderr (crash panics can land on either stream).
    fn combined(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.clone()
        } else if self.stdout.trim().is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

/// Run `program args…` under a wall-clock bound, capturing output.
///
/// Bounded via the `timeout` coreutil (sync std has no wait-with-timeout). In
/// the style of `discovery::run_kurtosis`, a missing tool is wrapped with a
/// clear "is it installed / on PATH?" message rather than a raw OS error.
fn sh_capture(program: &str, args: &[&str]) -> Result<ShellOutput> {
    let secs = Duration::from_secs(SHELL_TIMEOUT_SECS)
        .as_secs()
        .to_string();
    let output = Command::new("timeout")
        .arg(&secs)
        .arg(program)
        .args(args)
        .output()
        .wrap_err_with(|| {
            format!(
                "failed to spawn `{program}` (via `timeout`). Is `{program}` installed and on PATH?"
            )
        })?;

    // `timeout` exits 124 on timeout, 127 when the inner tool is not found.
    match output.status.code() {
        Some(124) => {
            return Err(eyre!(
                "`{program} {}` timed out after {secs}s",
                args.join(" ")
            ));
        }
        Some(127) => {
            return Err(eyre!(
                "`{program}` not found on PATH (needed by `sim triage`)"
            ));
        }
        _ => {}
    }

    Ok(ShellOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSPECT: &str = include_str!("../../../tests/fixtures/enclave_inspect.txt");

    #[test]
    fn parses_status_column_keeping_running_and_stopped() {
        let statuses = parse_service_statuses(INSPECT);
        assert_eq!(statuses.len(), 2, "two user services, got {statuses:?}");

        let running = statuses
            .iter()
            .find(|s| s.name == "cl-1-lighthouse-geth")
            .expect("running service present");
        assert_eq!(running.status, "RUNNING");

        let stopped = statuses
            .iter()
            .find(|s| s.name == "mev-relay-helix")
            .expect("stopped service present");
        assert_eq!(stopped.status, "STOPPED");
    }

    #[test]
    fn services_to_triage_picks_non_running() {
        let statuses = parse_service_statuses(INSPECT);
        let targets = services_to_triage(&statuses, &[]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "mev-relay-helix");
        assert_eq!(targets[0].status, "STOPPED");
    }

    #[test]
    fn services_to_triage_adds_unregistered_known_crash() {
        // A service the launch tried to add but that aborted before registering:
        // inspect never lists it, yet we still want it triaged.
        let statuses = parse_service_statuses(INSPECT);
        let targets = services_to_triage(&statuses, &["ghost-service", "cl-1-lighthouse-geth"]);
        // stopped mev-relay + the unregistered ghost; the RUNNING known name is
        // already listed, so it is not double-added.
        let names: Vec<&str> = targets.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"mev-relay-helix"));
        assert!(names.contains(&"ghost-service"));
        assert!(!names.contains(&"cl-1-lighthouse-geth"));
        let ghost = targets.iter().find(|s| s.name == "ghost-service").unwrap();
        assert_eq!(ghost.status, "UNREGISTERED");
    }
}
