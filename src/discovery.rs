//! Kurtosis enclave service discovery.
//!
//! Discovers services running in a Kurtosis enclave by shelling out to the
//! kurtosis CLI and parsing its output.
//!
//! TODO: Replace shell invocations with a Kurtosis Rust SDK when one exists.
//! Currently no official Rust SDK is available. The Go SDK lives at
//! github.com/kurtosis-tech/kurtosis/api/golang.

use std::process::Command;

use eyre::{Result, WrapErr, bail};
use tracing::{debug, info, warn};

/// A single payload-delivered record from post-mortem Postgres query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PostMortemRecord {
    pub slot: u64,
    pub block_hash: String,
    pub value: String,
}

/// Discovered services from a Kurtosis enclave.
#[derive(Debug, Default)]
pub struct EnclaveServices {
    pub beacon_urls: Vec<String>,
    pub relay_urls: Vec<String>,
    pub cb_pbs_urls: Vec<String>,
    pub cb_metrics_urls: Vec<String>,
    pub cb_service_names: Vec<String>,
    pub prometheus_url: Option<String>,
}

/// Run a kurtosis CLI command and return stdout.
fn run_kurtosis(args: &[&str]) -> Result<String> {
    let cmd_str = format!("kurtosis {}", args.join(" "));
    debug!("Running: {cmd_str}");

    let output = Command::new("kurtosis")
        .args(args)
        .output()
        .wrap_err("kurtosis CLI not found. Is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "kurtosis command failed (rc={}): {}",
            output.status,
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Get a clean URL for a specific service port via `kurtosis port print`.
fn port_print(enclave: &str, service: &str, port_name: &str) -> Option<String> {
    match run_kurtosis(&["port", "print", enclave, service, port_name]) {
        Ok(output) => {
            let url = output.trim().to_string();
            if url.is_empty() {
                return None;
            }
            Some(if url.starts_with("http") {
                url
            } else {
                format!("http://{url}")
            })
        }
        Err(e) => {
            debug!("port print failed for {service}/{port_name}: {e}");
            None
        }
    }
}

/// A parsed service from kurtosis inspect output.
struct ParsedService {
    name: String,
    ports: Vec<(String, String)>, // (port_name, url)
}

/// Parse `kurtosis enclave inspect` output to extract service names and port URLs.
fn parse_services(inspect_output: &str) -> Vec<ParsedService> {
    let mut services = Vec::new();
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

        // Split on 2+ whitespace to get columns
        let parts: Vec<&str> = split_on_multi_space(stripped);
        if parts.len() < 3 {
            continue;
        }

        let service_name = parts[1].trim().to_string();
        if service_name.is_empty() {
            continue;
        }

        let rest = parts[2..].join("  ");
        let ports = extract_ports(&rest);

        services.push(ParsedService {
            name: service_name,
            ports,
        });
    }

    services
}

/// Split a string on runs of 2+ whitespace characters.
fn split_on_multi_space(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = None;
    let mut space_count = 0;

    for (i, c) in s.char_indices() {
        if c == ' ' || c == '\t' {
            space_count += 1;
            if space_count == 2 && start.is_some() {
                result.push(&s[start.unwrap()..i - 1]);
                start = None;
            }
        } else {
            if start.is_none() {
                start = Some(i);
            }
            space_count = 0;
        }
    }
    if let Some(s_start) = start {
        result.push(&s[s_start..]);
    }
    result
}

/// Extract port mappings from a string like:
/// `http: 4000/tcp -> http://127.0.0.1:32811  metrics: 8080/tcp -> http://127.0.0.1:32812`
fn extract_ports(s: &str) -> Vec<(String, String)> {
    let mut ports = Vec::new();
    // Look for patterns: word: digits/tcp -> url
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Find "word:" pattern
        if let Some(colon_pos) = s[i..].find(':') {
            let abs_colon = i + colon_pos;
            // Walk back to find port name start
            let name_start = s[i..abs_colon]
                .rfind(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .map(|p| i + p + 1)
                .unwrap_or(i);
            let port_name = s[name_start..abs_colon].trim();

            if port_name.is_empty() {
                i = abs_colon + 1;
                continue;
            }

            // Look for "-> http" after the colon
            if let Some(arrow_offset) = s[abs_colon..].find("-> ") {
                let url_start = abs_colon + arrow_offset + 3;
                // Require a "/tcp" or "/udp" transport token between the
                // colon and the arrow so we don't accidentally treat stray
                // `word:` tokens (e.g. label prefixes) as port definitions.
                let between = &s[abs_colon..url_start];
                if !between.contains("/tcp") && !between.contains("/udp") {
                    i = abs_colon + 1;
                    continue;
                }
                // URL ends at whitespace or end of string
                let url_end = s[url_start..]
                    .find(|c: char| c.is_whitespace())
                    .map(|p| url_start + p)
                    .unwrap_or(len);
                let url = s[url_start..url_end].trim();

                if url.starts_with("http") {
                    ports.push((port_name.to_string(), url.to_string()));
                }
                i = url_end;
            } else {
                i = abs_colon + 1;
            }
        } else {
            break;
        }
    }

    ports
}

/// Match a service name against a glob pattern supporting `*` anywhere.
///
/// Splits the pattern on `*` and requires each literal segment to appear in
/// order in `name`. Anchoring: first segment must match at start unless the
/// pattern begins with `*`; last segment must match at end unless pattern
/// ends with `*`.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return name == pattern;
    }
    let segs: Vec<&str> = pattern.split('*').collect();
    let starts_anywhere = pattern.starts_with('*');
    let ends_anywhere = pattern.ends_with('*');

    let mut cursor = 0usize;
    for (i, seg) in segs.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 && !starts_anywhere {
            if !name[cursor..].starts_with(seg) {
                return false;
            }
            cursor += seg.len();
        } else if i == segs.len() - 1 && !ends_anywhere {
            // Last segment must match at end.
            if !name[cursor..].ends_with(seg) {
                return false;
            }
            // Also ensure it occurs after cursor.
            let tail_pos = name.len().saturating_sub(seg.len());
            if tail_pos < cursor {
                return false;
            }
            cursor = name.len();
        } else {
            match name[cursor..].find(seg) {
                Some(off) => cursor += off + seg.len(),
                None => return false,
            }
        }
    }
    true
}

/// Discover all relevant services in a Kurtosis enclave.
pub fn discover(enclave: &str) -> Result<EnclaveServices> {
    let mut result = EnclaveServices::default();

    let inspect_output = run_kurtosis(&["enclave", "inspect", "--full-uuids", enclave])
        .wrap_err_with(|| format!("Could not inspect enclave '{enclave}'"))?;

    let services = parse_services(&inspect_output);
    if services.is_empty() {
        warn!("No services found in enclave '{enclave}'");
        return Ok(result);
    }

    info!("Found {} services in enclave '{enclave}'", services.len());

    for svc in &services {
        let find_port = |name: &str| -> Option<String> {
            svc.ports
                .iter()
                .find(|(pn, _)| pn == name)
                .map(|(_, url)| url.clone())
        };

        // Beacon API: cl-* services, port 'http'
        if matches_pattern(&svc.name, "cl-*") {
            let url = port_print(enclave, &svc.name, "http").or_else(|| find_port("http"));
            if let Some(url) = url {
                info!("Beacon API: {} -> {url}", svc.name);
                result.beacon_urls.push(url);
            } else {
                warn!("Beacon '{}': no http port", svc.name);
            }
        }

        // Relay Data API: mev-relay-*-api or mev-relay-api
        if matches_pattern(&svc.name, "mev-relay-*") && svc.name.ends_with("-api")
            || svc.name == "mev-relay-api"
        {
            let url = port_print(enclave, &svc.name, "http").or_else(|| find_port("http"));
            if let Some(url) = url {
                info!("Relay API: {} -> {url}", svc.name);
                result.relay_urls.push(url);
            } else {
                warn!("Relay '{}': no http port", svc.name);
            }
        }

        // Commit-Boost: commit-boost-* services
        //
        // The kurtosis ethereum-package publishes CB's PBS port under the
        // name "http" (port 18550), not "pbs". Metrics are only exposed
        // when commit_boost_config enables [metrics] AND the yaml publishes
        // the port -- see configs/pbs-metrics.yml. Absent that, matrix
        // checks will SKIP gracefully.
        if matches_pattern(&svc.name, "commit-boost-*") {
            result.cb_service_names.push(svc.name.clone());

            // Try "pbs" first (older configs / custom setups), fall back to
            // "http" (ethereum-package default).
            let pbs_url = port_print(enclave, &svc.name, "pbs")
                .or_else(|| find_port("pbs"))
                .or_else(|| port_print(enclave, &svc.name, "http"))
                .or_else(|| find_port("http"));
            if let Some(url) = pbs_url {
                info!("CB PBS: {} -> {url}", svc.name);
                result.cb_pbs_urls.push(url);
            } else {
                warn!("CB '{}': no pbs/http port exposed", svc.name);
            }

            // Metrics port may be named "metrics" or "http-metrics".
            let metrics_url = port_print(enclave, &svc.name, "metrics")
                .or_else(|| find_port("metrics"))
                .or_else(|| port_print(enclave, &svc.name, "http-metrics"))
                .or_else(|| find_port("http-metrics"));
            if let Some(url) = metrics_url {
                info!("CB metrics: {} -> {url}", svc.name);
                result.cb_metrics_urls.push(url);
            }
        }

        // Prometheus
        if svc.name == "prometheus" {
            if let Some(url) = port_print(enclave, &svc.name, "http").or_else(|| find_port("http"))
            {
                info!("Prometheus: {} -> {url}", svc.name);
                result.prometheus_url = Some(url);
            } else {
                warn!("Prometheus service found but no http port available.");
            }
        }
    }

    info!(
        "Discovery: beacons={}, relays={}, cb_pbs={}, cb_metrics={}, cb_names={}, prometheus={}",
        result.beacon_urls.len(),
        result.relay_urls.len(),
        result.cb_pbs_urls.len(),
        result.cb_metrics_urls.len(),
        result.cb_service_names.len(),
        if result.prometheus_url.is_some() {
            "yes"
        } else {
            "no"
        },
    );

    if result.beacon_urls.is_empty() {
        warn!("No beacon API services (cl-*) found");
    }
    if result.relay_urls.is_empty() {
        warn!("No relay API services found");
    }
    if result.cb_service_names.is_empty() {
        warn!("No Commit-Boost services found");
    }

    Ok(result)
}

/// Post-mortem: query `mev-relay-postgres` directly when the relay API is dead.
///
/// Shells out to `kurtosis service exec` to run a psql query inside the relay's
/// Postgres container. Returns payload-delivered records found. If the Postgres
/// container doesn't exist or the query fails, returns an empty Vec.
///
/// This salvages a verdict when the relay crashed mid-run: if the pipeline
/// worked before the crash, Postgres still has the evidence.
pub fn query_mev_relay_postgres(enclave: &str) -> Vec<PostMortemRecord> {
    debug!("Running post-mortem Postgres query in enclave '{enclave}'");

    let query = "SELECT slot, block_hash, value FROM mainnet_payload_delivered ORDER BY slot DESC LIMIT 20;";

    // `kurtosis service exec` returns output on stdout. The psql output
    // includes a header row and separator line before data rows.
    match Command::new("kurtosis")
        .args([
            "service",
            "exec",
            enclave,
            "mev-relay-postgres",
            "--",
            "psql",
            "-U",
            "postgres",
            "-d",
            "mev_boost_relay",
            "-c",
            query,
        ])
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                debug!(
                    "Post-mortem query failed (exit {}): {}",
                    output.status,
                    stderr.trim()
                );
                return Vec::new();
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_postmortem_output(&stdout)
        }
        Err(e) => {
            debug!("Post-mortem: kurtosis CLI not available: {e}");
            Vec::new()
        }
    }
}

/// Parse psql tabular output into PostMortemRecords.
///
/// Expected format:
/// ```text
///  slot | block_hash | value
/// ------+------------+-------
///   224 | 0xabc...   | 12345
///   223 | 0xdef...   | 67890
/// (2 rows)
/// ```
///
/// Skips header rows (first 2 lines: column names + separator) and the
/// trailing "(N rows)" line.
fn parse_postmortem_output(output: &str) -> Vec<PostMortemRecord> {
    let mut records = Vec::new();
    let mut seen_header = false;
    let mut seen_sep = false;

    for line in output.lines() {
        let stripped = line.trim();

        if stripped.is_empty() {
            continue;
        }

        // Skip the column-name header and separator line.
        if !seen_header {
            seen_header = true;
            continue;
        }
        if !seen_sep {
            seen_sep = true;
            continue;
        }

        // Skip trailing "(N rows)" line.
        if stripped.starts_with('(') && stripped.ends_with(')') {
            continue;
        }

        // Data rows: " 224 | 0xabc...   | 12345"
        let parts: Vec<&str> = stripped.split('|').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            continue;
        }

        let slot: u64 = match parts[0].parse() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let block_hash = parts[1].to_string();
        let value = parts[2].to_string();

        records.push(PostMortemRecord {
            slot,
            block_hash,
            value,
        });
    }

    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_pattern() {
        assert!(matches_pattern("cl-1-lighthouse-geth", "cl-*"));
        assert!(matches_pattern("mev-relay-api", "mev-relay-api"));
        assert!(matches_pattern("mev-relay-0-api", "mev-relay-*"));
        assert!(!matches_pattern("prometheus", "cl-*"));
        assert!(matches_pattern("commit-boost-001", "commit-boost-*"));
    }

    #[test]
    fn test_extract_ports() {
        let input =
            "http: 4000/tcp -> http://127.0.0.1:32811  metrics: 8080/tcp -> http://127.0.0.1:32812";
        let ports = extract_ports(input);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].0, "http");
        assert!(ports[0].1.contains("32811"));
        assert_eq!(ports[1].0, "metrics");
        assert!(ports[1].1.contains("32812"));
    }

    #[test]
    fn test_split_on_multi_space() {
        let parts = split_on_multi_space(
            "abc123   cl-1-lighthouse   http: 4000/tcp -> http://localhost:32811   RUNNING",
        );
        assert!(parts.len() >= 3);
        assert_eq!(parts[1], "cl-1-lighthouse");
    }

    #[test]
    fn test_parse_postmortem_output() {
        let output = concat!(
            " slot | block_hash | value\n",
            "------+------------+-------\n",
            "  224 | 0xabc123  | 12345\n",
            "  223 | 0xdef456  | 67890\n",
            "(2 rows)\n",
        );
        let records = parse_postmortem_output(output);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].slot, 224);
        assert_eq!(records[0].block_hash, "0xabc123");
        assert_eq!(records[0].value, "12345");
        assert_eq!(records[1].slot, 223);
        assert_eq!(records[1].block_hash, "0xdef456");
        assert_eq!(records[1].value, "67890");
    }

    #[test]
    fn test_parse_postmortem_output_empty() {
        let output = concat!(
            " slot | block_hash | value\n",
            "------+------------+-------\n",
            "(0 rows)\n",
        );
        let records = parse_postmortem_output(output);
        assert!(records.is_empty());
    }
}
