//! `sim preflight <args-file>` — the config-drift gate (Task 3), HELIX ONLY.
//!
//! Extract the two embedded config blocks (`render`), validate the HELIX block by
//! running the real helix image against it (~1s), and emit a **3-valued** verdict.
//! The 3-value part is a hard requirement: a slow image pull, docker being down,
//! or a pre-genesis runtime panic must NOT be scored as config drift ("pilot
//! breaks the instrument"). The CB block is stubbed `Inconclusive` — typed
//! validation lands in P2.
//!
//! Layering, mirroring `triage`: the classifier (`classify_helix_probe`) is PURE
//! and fixture-tested; the process I/O (`preflight_helix`, `run`) is smoke-checked
//! manually with Docker + the real image, NOT in `cargo test`.
//!
//! Why key on the panic LOCATION, not "reached fetch": the same run can both
//! reach the fetch stage AND later panic for a non-config reason, and a
//! config-parse failure has a stable, recognisable location (`config.rs` / a
//! serde parse error). Keying on location lets us separate genuine schema drift
//! (Fail) from env/timing/infra noise (Inconclusive) rather than guessing from
//! how far the process got.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use serde::Serialize;

use crate::diagnose::{CauseKind, extract_root_cause};
use crate::render::{self, default_dummies};

/// Wall-clock bound for the helix config probe. Config parse + reaching the
/// beacon-fetch stage takes ~1s; a clean config then blocks on the (absent)
/// beacon, which we cut off. helix IGNORES SIGTERM, so we `timeout --signal=KILL`
/// to stop the probe promptly rather than let it run to its own 1-minute panic.
const PROBE_TIMEOUT_SECS: u64 = 8;

/// The 3-valued config verdict for one block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ConfigVerdict {
    /// The config parsed cleanly (the probe got past config load).
    Pass,
    /// Genuine config drift: a config-parse panic naming the offending field.
    Fail { field: String, detail: String },
    /// Could not decide (env/timing/infra) — must NOT fail the gate.
    Inconclusive { reason: String },
}

/// The preflight report: one verdict per config block.
#[derive(Debug, Clone, Serialize)]
pub struct PreflightReport {
    pub helix: ConfigVerdict,
    pub commit_boost: ConfigVerdict,
}

/// Classify a helix config probe's outcome into a 3-valued verdict.
///
/// Keys on the panic LOCATION (via `diagnose::extract_root_cause`), not on how
/// far the process got:
/// - a `config.rs` panic / serde parse error → `Fail` (real config drift), with
///   the offending field captured from the message.
/// - reached the beacon-fetch stage with no config panic → `Pass` (config
///   parsed; the missing beacon is expected in a probe).
/// - a pre-genesis panic (`chain_info.rs` / `HousekeeperTile` / `current_slot` /
///   unwrap-on-None), a missing `GENESIS_*` env, a kill, or an infra signal
///   (image-pull error, docker daemon down, timeout) → `Inconclusive`.
/// - nothing recognisable → `Inconclusive`.
pub fn classify_helix_probe(exit_status: Option<i32>, logs: &str) -> ConfigVerdict {
    let cause = extract_root_cause(logs);

    // Panic-keyed buckets. ORDER MATTERS: `config.rs` hosts BOTH the serde config
    // parse AND env-var reads / runtime construction, so the non-drift buckets
    // (env-missing, pre-genesis) must be checked BEFORE the config-drift Fail —
    // otherwise a missing `RELAY_KEY` env read (which panics inside config.rs) is
    // mis-scored as schema drift. The "pilot breaks the instrument" trap.
    if let Some(rc) = &cause {
        let loc = rc.location.as_deref().unwrap_or("");
        let msg = &rc.message;

        // 1) A missing env PREREQUISITE (e.g. `RELAY_KEY should be set:
        //    NotPresent`) — happens inside config.rs but is not schema drift.
        if is_env_missing(msg) {
            return ConfigVerdict::Inconclusive {
                reason: format!("missing env prerequisite (not config drift): {msg}"),
            };
        }

        // 2) Pre-genesis / runtime panic (HousekeeperTile current_slot unwrap) —
        //    NOT config drift.
        let pregenesis = loc.contains("chain_info.rs")
            || msg.contains("HousekeeperTile")
            || msg.contains("current_slot")
            || (msg.contains("unwrap()") && msg.contains("None"));
        if pregenesis {
            return ConfigVerdict::Inconclusive {
                reason: format!("pre-genesis runtime panic (not config): {msg}"),
            };
        }

        // 3) Real config drift: a serde parse signature, or a config.rs parse
        //    panic — captures the offending field name from the message.
        if is_serde_parse_error(msg) || loc.contains("config.rs") {
            return ConfigVerdict::Fail {
                field: capture_field(msg),
                detail: msg.clone(),
            };
        }
    }

    // 4) Config parsed and the relay reached the beacon-fetch stage → Pass. This
    //    is checked BEFORE the timeout/kill bucket: our probe supplies no beacon,
    //    so a config-clean relay reaches fetch and then either retries until our
    //    wall-clock kill or panics with "failed fetching chain info for 1 minute"
    //    — both mean the config parsed, so both are a Pass, not a kill.
    if reached_fetch(logs) && !is_config_panic(&cause) {
        return ConfigVerdict::Pass;
    }

    // 5) Infra signals in the logs — image pull, docker daemon, timeout marker.
    if let Some(reason) = infra_signal(logs) {
        return ConfigVerdict::Inconclusive { reason };
    }

    // 6) A bare kill (from the extracted cause or a SIGKILL/timeout exit code)
    //    with no fetch-stage evidence — we could not tell whether config parsed.
    if matches!(cause.as_ref().map(|c| c.kind), Some(CauseKind::Killed))
        || matches!(exit_status, Some(137) | Some(124) | Some(143))
    {
        return ConfigVerdict::Inconclusive {
            reason: "probe killed (timeout / OOM) before reaching a config verdict".to_string(),
        };
    }

    // 7) Missing genesis env — a runtime prerequisite, not a config schema issue.
    if logs.contains("GENESIS_") && logs.to_ascii_lowercase().contains("not set")
        || logs.contains("missing GENESIS")
    {
        return ConfigVerdict::Inconclusive {
            reason: "missing GENESIS_* env (runtime prerequisite, not config)".to_string(),
        };
    }

    // 8) Nothing recognisable.
    ConfigVerdict::Inconclusive {
        reason: "no recognisable config-parse or fetch-stage signal in probe logs".to_string(),
    }
}

/// Does this message look like a missing ENV prerequisite (e.g. `RELAY_KEY
/// should be set: NotPresent`)? Such reads live inside `config.rs` but are a
/// runtime prerequisite, not config-schema drift.
fn is_env_missing(message: &str) -> bool {
    const NEEDLES: [&str; 4] = [
        "NotPresent",
        "should be set",
        "environment variable",
        "VarError",
    ];
    NEEDLES.iter().any(|n| message.contains(n))
}

/// Does this message look like a serde/config parse failure (schema drift)?
fn is_serde_parse_error(message: &str) -> bool {
    const NEEDLES: [&str; 5] = [
        "missing field",
        "unknown field",
        "untagged",
        "failed to parse config",
        "did not match any variant",
    ];
    NEEDLES.iter().any(|n| message.contains(n))
}

/// True if the extracted cause is a config-parse panic (used to guard `Pass`).
fn is_config_panic(cause: &Option<crate::diagnose::RootCause>) -> bool {
    cause.as_ref().is_some_and(|rc| {
        rc.location.as_deref().unwrap_or("").contains("config.rs")
            || is_serde_parse_error(&rc.message)
    })
}

/// Capture the offending field name from a serde error message.
///
/// Serde renders the field between backticks (`missing field \`decoder\``), so we
/// pull the first backtick-quoted token. Pattern-based — a never-seen field is
/// captured the same as a known one. Falls back to `"unknown"`.
fn capture_field(message: &str) -> String {
    let mut parts = message.split('`');
    // parts: [before, FIELD, after, …] — the first quoted token is index 1.
    if let (Some(_), Some(field)) = (parts.next(), parts.next())
        && !field.is_empty()
    {
        return field.to_string();
    }
    "unknown".to_string()
}

/// Did the relay reach the beacon-fetch stage (proving config parsed)?
fn reached_fetch(logs: &str) -> bool {
    const NEEDLES: [&str; 3] = [
        "get_chain_info",
        "failed fetching chain info",
        "starting metrics server",
    ];
    NEEDLES.iter().any(|n| logs.contains(n))
}

/// Recognise an infra failure (image pull / docker daemon / timeout marker).
fn infra_signal(logs: &str) -> Option<String> {
    let lower = logs.to_ascii_lowercase();
    const NEEDLES: [(&str, &str); 6] = [
        (
            "manifest unknown",
            "image not available in registry (manifest unknown)",
        ),
        (
            "no such image",
            "image not present locally / pull failed (no such image)",
        ),
        (
            "unable to find image",
            "image not present locally / pull in progress",
        ),
        (
            "cannot connect to the docker daemon",
            "docker daemon not reachable",
        ),
        (
            "is the docker daemon running",
            "docker daemon not reachable",
        ),
        ("__sim_probe_timeout__", "probe hit the wall-clock timeout"),
    ];
    NEEDLES
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, reason)| reason.to_string())
}

/// Validate the helix config block by running the real helix image against it.
///
/// Substitutes the block's runtime vars, writes it to a tmp dir, runs
/// `docker run --rm --entrypoint sh <image> -c 'exec /app/helix-relay --config
/// /cfg/config.yaml'` with the dir mounted at `/cfg` under a bounded timeout,
/// captures combined output, and classifies. Smoke-checked manually (needs Docker
/// + the image), NOT a `cargo test`.
pub fn preflight_helix(image: &str, yaml_block: &str) -> Result<ConfigVerdict> {
    let rendered = render::substitute_runtime_vars(yaml_block, &default_dummies());

    // A private tmp dir mounted read-only into the container.
    let tmp = std::env::temp_dir().join(format!("sim-preflight-{}", std::process::id()));
    fs::create_dir_all(&tmp).wrap_err("create preflight tmp dir")?;
    let cfg_path = tmp.join("config.yaml");
    fs::write(&cfg_path, rendered).wrap_err("write rendered helix config")?;

    let container = format!("sim-preflight-{}", std::process::id());
    let verdict = run_probe(image, &tmp, &container);

    // Best-effort cleanup of the tmp file and any orphan container.
    let _ = fs::remove_file(&cfg_path);
    let _ = fs::remove_dir(&tmp);
    let _ = Command::new("docker")
        .args(["rm", "-f", &container])
        .output();

    verdict
}

/// The dummy env the real helix `:main` image reads at boot BEFORE it parses the
/// config file (from `helix_relay_launcher.star`). Without these the relay panics
/// on an env read inside `config.rs` and never reaches the YAML parse we want to
/// exercise. Values are the launcher's own dummies (a throwaway secret key, etc.);
/// a past `GENESIS_TIME` lets the relay compute `current_slot()` and reach the
/// beacon-fetch stage instead of the pre-genesis unwrap.
const PROBE_ENV: [(&str, &str); 4] = [
    (
        "RELAY_KEY",
        "0x607a11b45a7219cc61a3d9c5fd08c7eebd602a6a19a977f8d3771d5711a550f2",
    ),
    ("POSTGRES_PASSWORD", "postgres"),
    ("ADMIN_TOKEN", "admin_token"),
    ("GENESIS_TIME", "1700000000"),
];

/// Run the bounded docker probe and classify its combined output.
fn run_probe(image: &str, cfg_dir: &Path, container: &str) -> Result<ConfigVerdict> {
    let mount = format!("{}:/cfg:ro", cfg_dir.display());
    let secs = Duration::from_secs(PROBE_TIMEOUT_SECS)
        .as_secs()
        .to_string();

    // `timeout` bounds the shell (sync std has no wait-with-timeout), matching
    // `triage`. `--signal=KILL` because helix ignores SIGTERM; on the kill it
    // exits 137 (a plain timeout kill would be 124); a missing docker → 127.
    let mut cmd = Command::new("timeout");
    cmd.args(["--signal=KILL", &secs])
        .args(["docker", "run", "--rm", "--name", container, "-v", &mount]);
    for (k, v) in PROBE_ENV {
        cmd.args(["-e", &format!("{k}={v}")]);
    }
    let output = cmd
        .args(["--entrypoint", "sh", image, "-c"])
        .arg("exec /app/helix-relay --config /cfg/config.yaml")
        .output()
        .wrap_err("failed to spawn `docker` (via `timeout`). Is docker installed and on PATH?")?;

    if output.status.code() == Some(127) {
        return Ok(ConfigVerdict::Inconclusive {
            reason: "docker not found on PATH (needed by `sim preflight`)".to_string(),
        });
    }

    let mut logs = String::new();
    logs.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        logs.push('\n');
        logs.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    // On a wall-clock kill (124 plain / 137 SIGKILL / no code = signal), mark the
    // logs so the classifier can distinguish a timeout from a clean exit.
    if matches!(output.status.code(), Some(124) | Some(137) | None) {
        logs.push_str("\n__sim_probe_timeout__\n");
    }

    Ok(classify_helix_probe(output.status.code(), &logs))
}

/// Entry point for `sim preflight <args-file>`.
///
/// Extracts both config blocks, validates the helix block against its image,
/// stubs the CB block as `Inconclusive` (P2), prints the JSON report, and exits
/// nonzero ONLY on a `Fail` (an `Inconclusive` must not break the gate on a slow
/// pull).
pub fn run(args_file: &Path) -> Result<()> {
    let contents = fs::read_to_string(args_file)
        .wrap_err_with(|| format!("read args-file {}", args_file.display()))?;
    let blocks = render::extract_config_blocks(&contents)?;
    let image = helix_image(&contents)?;

    tracing::info!(image = %image, "preflighting helix config");
    let helix = preflight_helix(&image, &blocks.helix)?;

    // The CB block IS extracted, but typed validation is deferred to P2 (a
    // 409-crate dep, broken as specified). Surface that we saw it, then stub.
    tracing::info!(
        cb_config_bytes = blocks.commit_boost.len(),
        "commit-boost config extracted; typed validation deferred to P2"
    );
    let commit_boost = ConfigVerdict::Inconclusive {
        reason: "typed validation lands in P2".to_string(),
    };

    let report = PreflightReport {
        helix,
        commit_boost,
    };
    let json = serde_json::to_string_pretty(&report).wrap_err("serialize preflight report")?;
    println!("{json}");

    // Exit nonzero ONLY on a genuine Fail — Inconclusive must not fail the gate.
    if matches!(report.helix, ConfigVerdict::Fail { .. })
        || matches!(report.commit_boost, ConfigVerdict::Fail { .. })
    {
        std::process::exit(1);
    }
    Ok(())
}

/// Read the helix image id from the args-file (`mev_params.helix_relay_image`).
fn helix_image(args_file_contents: &str) -> Result<String> {
    let root: serde_yaml::Value = serde_yaml::from_str(args_file_contents)?;
    root.get("mev_params")
        .and_then(|m| m.get("helix_relay_image"))
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| eyre!("args-file has no `mev_params.helix_relay_image`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERDE_MISSING: &str =
        include_str!("../../../tests/fixtures/helix_serde_missing_field.log");
    const PREGENESIS: &str = include_str!("../../../tests/fixtures/helix_pregenesis_unwrap.log");
    const INVENTED: &str = include_str!("../../../tests/fixtures/invented_field.log");
    const REACHED_FETCH: &str = include_str!("../../../tests/fixtures/helix_reached_fetch.log");
    const DOCKER_PULL: &str = include_str!("../../../tests/fixtures/docker_pull_error.log");
    const OOM: &str = include_str!("../../../tests/fixtures/oom_killed.log");
    const CLEAN: &str = include_str!("../../../tests/fixtures/clean.log");

    #[test]
    fn serde_missing_field_is_fail_naming_the_field() {
        let v = classify_helix_probe(Some(101), SERDE_MISSING);
        match v {
            ConfigVerdict::Fail { field, .. } => {
                // Captured from the message (generalization), not hardcoded-matched.
                assert_eq!(field, "decoder", "should capture the missing field name");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn held_out_field_is_captured_as_fail() {
        // A field name we have never seen — proves pattern capture, not memory.
        let v = classify_helix_probe(Some(101), INVENTED);
        match v {
            ConfigVerdict::Fail { field, .. } => assert_eq!(field, "foobar"),
            other => panic!("expected Fail for held-out field, got {other:?}"),
        }
    }

    #[test]
    fn pregenesis_unwrap_is_inconclusive_not_fail() {
        // The "pilot breaks the instrument" trap: a runtime panic must NOT be
        // scored as config drift.
        let v = classify_helix_probe(Some(101), PREGENESIS);
        assert!(
            matches!(v, ConfigVerdict::Inconclusive { .. }),
            "pre-genesis unwrap must be Inconclusive, got {v:?}"
        );
    }

    #[test]
    fn reached_fetch_is_pass() {
        // Config parsed; the relay got to the beacon-fetch stage. The missing
        // beacon in a probe is expected — that is a Pass, not a failure.
        let v = classify_helix_probe(None, REACHED_FETCH);
        assert_eq!(v, ConfigVerdict::Pass);
    }

    #[test]
    fn docker_pull_error_is_inconclusive() {
        // An image-pull / registry problem is infra, not config drift.
        let v = classify_helix_probe(Some(125), DOCKER_PULL);
        assert!(
            matches!(v, ConfigVerdict::Inconclusive { .. }),
            "docker pull error must be Inconclusive, got {v:?}"
        );
    }

    #[test]
    fn oom_kill_is_inconclusive() {
        let v = classify_helix_probe(Some(137), OOM);
        assert!(
            matches!(v, ConfigVerdict::Inconclusive { .. }),
            "a kill must be Inconclusive, got {v:?}"
        );
    }

    #[test]
    fn unrecognised_logs_are_inconclusive() {
        let v = classify_helix_probe(None, CLEAN);
        assert!(
            matches!(v, ConfigVerdict::Inconclusive { .. }),
            "no recognisable signal must be Inconclusive, got {v:?}"
        );
    }

    #[test]
    fn timeout_exit_is_inconclusive() {
        let v = classify_helix_probe(Some(124), "some partial output\n__sim_probe_timeout__\n");
        assert!(
            matches!(v, ConfigVerdict::Inconclusive { .. }),
            "a timeout kill must be Inconclusive, got {v:?}"
        );
    }
}
