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

/// Derive a relay identity from the Kurtosis service name.
///
/// Returns a short string like "helix", "flashbots", or "mev-rs"
/// that can be matched against mux entry IDs.
pub fn relay_identity(service_name: &str) -> Option<String> {
    let lower = service_name.to_lowercase();
    if lower.contains("helix") {
        Some("helix".to_string())
    } else if lower.contains("mev-rs") {
        Some("mev-rs".to_string())
    } else if lower.contains("relay") {
        // Generic mev-boost relay (used by flashbots)
        Some("flashbots".to_string())
    } else {
        None
    }
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
pub struct ParsedService {
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
///
/// Public so other bins (e.g. `sim triage`) can reuse the exact column split
/// used to read the kurtosis `User Services` table, rather than re-deriving it.
pub fn split_on_multi_space(s: &str) -> Vec<&str> {
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
    let inspect_output = run_kurtosis(&["enclave", "inspect", "--full-uuids", enclave])
        .wrap_err_with(|| format!("Could not inspect enclave '{enclave}'"))?;

    let services = parse_services(&inspect_output);
    if services.is_empty() {
        warn!("No services found in enclave '{enclave}'");
        return Ok(EnclaveServices::default());
    }
    info!("Found {} services in enclave '{enclave}'", services.len());

    // The only IO in the selection below: a `kurtosis port print` fallback for
    // ports the inspect-table parse missed.
    Ok(classify_services(&services, |svc, port| {
        port_print(enclave, svc, port)
    }))
}

/// Decide which discovered services are the beacon nodes, relay APIs, CB
/// sidecars and prometheus, and pick a URL for each (pure, Law 4 seam).
///
/// `port_fallback(service, port_name)` is consulted ONLY when the port was not
/// already parsed out of the `enclave inspect` table; tests pass a stub, the
/// real caller passes `kurtosis port print`. Splitting it this way makes the
/// classification - which decides WHAT gets checked, and therefore silently
/// invalidates every downstream check when it is wrong - testable without a
/// live enclave. It matters more since Law 7: service names carry the client
/// pair (`cl-1-prysm-nethermind` vs `cl-1-lighthouse-geth`), so the patterns
/// must not accidentally encode one pair.
pub fn classify_services(
    services: &[ParsedService],
    port_fallback: impl Fn(&str, &str) -> Option<String>,
) -> EnclaveServices {
    let mut result = EnclaveServices::default();

    for svc in services {
        let find_port = |name: &str| -> Option<String> {
            svc.ports
                .iter()
                .find(|(pn, _)| pn == name)
                .map(|(_, url)| url.clone())
        };
        // Try EVERY already-parsed port name first, and only then fall back to
        // the injected lookup. Interleaving them per-name would shell out to
        // `kurtosis port print` for an early name before trying a later name
        // that the inspect table already carried - a subprocess per relay per
        // run, which is the precedence bug an earlier perf pass removed.
        let pick = |names: &[&str]| -> Option<String> {
            names
                .iter()
                .find_map(|n| find_port(n))
                .or_else(|| names.iter().find_map(|n| port_fallback(&svc.name, n)))
        };

        if matches_pattern(&svc.name, "cl-*") {
            match pick(&["http"]) {
                Some(url) => {
                    info!("Beacon API: {} -> {url}", svc.name);
                    result.beacon_urls.push(url);
                }
                None => warn!("Beacon '{}': no http port", svc.name),
            }
        }

        // Relay implementations differ in service name AND port id:
        //   flashbots "mev-relay-api" http/9067, helix "helix-relay"
        //   endpoint/4040, mev-rs "mev-rs-relay" http/28545. Supporting
        //   services (postgres/redis/website/housekeeper) are excluded.
        if is_relay_api_service(&svc.name) {
            match pick(&["http", "endpoint"]) {
                Some(url) => {
                    let identity =
                        relay_identity(&svc.name).unwrap_or_else(|| "unknown".to_string());
                    info!("Relay API: {} -> {url} (identity={identity})", svc.name);
                    result.relay_urls.push(url);
                }
                None => warn!("Relay '{}': no http/endpoint port", svc.name),
            }
        }

        if matches_pattern(&svc.name, "commit-boost-*") {
            result.cb_service_names.push(svc.name.clone());
            // "pbs" first (older/custom configs), then "http" (the
            // ethereum-package default, 18550).
            match pick(&["pbs", "http"]) {
                Some(url) => {
                    info!("CB PBS: {} -> {url}", svc.name);
                    result.cb_pbs_urls.push(url);
                }
                None => warn!("CB '{}': no pbs/http port exposed", svc.name),
            }
            // Metrics only exist when commit_boost_config enables [metrics] AND
            // the yaml publishes the port; absent that the matrix checks SKIP.
            if let Some(url) = pick(&["metrics", "http-metrics"]) {
                info!("CB metrics: {} -> {url}", svc.name);
                result.cb_metrics_urls.push(url);
            }
        }

        if svc.name == "prometheus" {
            match pick(&["http"]) {
                Some(url) => {
                    info!("Prometheus: {} -> {url}", svc.name);
                    result.prometheus_url = Some(url);
                }
                None => warn!("Prometheus service found but no http port available."),
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

    result
}

/// Heuristic check: is this service name a relay API endpoint?
///
/// Returns true if the name contains "relay" and does not match known
/// non-API relay services (postgres, redis, website, housekeeper).
/// This covers flashbots ("mev-relay-api"), helix ("helix-relay"),
/// mev-rs ("mev-rs-relay"), and any future relay implementations.
fn is_relay_api_service(name: &str) -> bool {
    let lower = name.to_lowercase();
    let known_non_api = ["-postgres", "-redis", "-website", "-housekeeper"];
    let is_relay = lower.contains("relay");
    // CONTAINS, not ends_with: the N-relay-instance topology suffixes every
    // service with its index, so the support services are named
    // `helix-relay-postgres-2`, which does not END with "-postgres". With
    // ends_with, a relay's POSTGRES container was classified as a relay data
    // API (found by test, 2026-08-04).
    let is_non_api = known_non_api.iter().any(|marker| lower.contains(marker));
    is_relay && !is_non_api
}

#[cfg(test)]
mod tests {
    use super::relay_identity;

    fn svc(name: &str, ports: &[(&str, &str)]) -> ParsedService {
        ParsedService {
            name: name.to_string(),
            ports: ports
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }

    /// A fallback that never resolves anything. NOT a panic-stub: the fallback
    /// is legitimately consulted for OPTIONAL lookups (CB metrics are absent in
    /// the default devnet shape). Precedence is asserted separately, by
    /// counting calls, in `parsed_ports_win_over_the_fallback`.
    fn no_fallback(_svc: &str, _port: &str) -> Option<String> {
        None
    }

    #[test]
    fn parsed_ports_win_over_the_fallback() {
        // The perf contract: a port already in the inspect table must never
        // cost a `kurtosis port print` subprocess. Counted, not asserted by
        // panic, so optional lookups (metrics) do not confuse the signal.
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let out = classify_services(
            &[svc("cl-1-lighthouse-geth", &[("http", "http://parsed:1")])],
            |_, _| {
                calls.set(calls.get() + 1);
                Some("http://shelled-out:1".to_string())
            },
        );
        assert_eq!(out.beacon_urls, vec!["http://parsed:1"]);
        assert_eq!(calls.get(), 0, "parsed port must not shell out");
    }

    #[test]
    fn relay_endpoint_port_costs_no_subprocess() {
        // Helix exposes only "endpoint". Trying "http" first must NOT shell out
        // before "endpoint" is tried against the parsed table - that was a real
        // regression (one subprocess per relay per run).
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let out = classify_services(
            &[svc("helix-relay-2", &[("endpoint", "http://h:1")])],
            |_, _| {
                calls.set(calls.get() + 1);
                None
            },
        );
        assert_eq!(out.relay_urls, vec!["http://h:1"]);
        assert_eq!(
            calls.get(),
            0,
            "parsed 'endpoint' must not cost a port print"
        );
    }

    #[test]
    fn classify_picks_beacon_relay_cb_and_prometheus() {
        let services = vec![
            svc("cl-1-lighthouse-geth", &[("http", "http://127.0.0.1:1111")]),
            svc("el-1-geth-lighthouse", &[("rpc", "http://127.0.0.1:2222")]),
            svc("helix-relay-2", &[("endpoint", "http://127.0.0.1:3333")]),
            svc(
                "commit-boost-1-lighthouse-geth",
                &[("http", "http://127.0.0.1:4444")],
            ),
            svc("prometheus", &[("http", "http://127.0.0.1:5555")]),
        ];
        let out = classify_services(&services, no_fallback);
        assert_eq!(out.beacon_urls, vec!["http://127.0.0.1:1111"]);
        assert_eq!(
            out.relay_urls,
            vec!["http://127.0.0.1:3333"],
            "helix uses 'endpoint'"
        );
        assert_eq!(out.cb_pbs_urls, vec!["http://127.0.0.1:4444"]);
        assert_eq!(out.cb_service_names, vec!["commit-boost-1-lighthouse-geth"]);
        assert_eq!(out.prometheus_url.as_deref(), Some("http://127.0.0.1:5555"));
        // The EL is not a beacon, a relay, or a CB service.
        assert_eq!(out.beacon_urls.len(), 1);
    }

    #[test]
    fn classify_is_client_pair_agnostic() {
        // Law 7: service names carry the pair. Patterns must not encode one.
        let lh = classify_services(
            &[svc("cl-1-lighthouse-geth", &[("http", "http://a:1")])],
            no_fallback,
        );
        let prysm = classify_services(
            &[svc("cl-1-prysm-nethermind", &[("http", "http://b:1")])],
            no_fallback,
        );
        assert_eq!(lh.beacon_urls.len(), 1);
        assert_eq!(
            prysm.beacon_urls.len(),
            1,
            "prysm+nethermind must classify too"
        );
    }

    #[test]
    fn classify_finds_every_relay_flavour_and_excludes_support_services() {
        let services = vec![
            svc("mev-relay-api", &[("http", "http://f:1")]),
            svc("helix-relay-2", &[("endpoint", "http://h:1")]),
            svc("mev-rs-relay", &[("http", "http://m:1")]),
            // Supporting services that must NOT be treated as relay APIs:
            svc("helix-relay-postgres-2", &[("http", "http://p:1")]),
            svc("mev-relay-website", &[("http", "http://w:1")]),
            svc("mev-relay-housekeeper", &[("http", "http://k:1")]),
        ];
        let out = classify_services(&services, no_fallback);
        assert_eq!(
            out.relay_urls.len(),
            3,
            "3 relay APIs, 3 support services excluded"
        );
        assert!(out.relay_urls.contains(&"http://h:1".to_string()));
        assert!(!out.relay_urls.contains(&"http://p:1".to_string()));
    }

    #[test]
    fn classify_prefers_pbs_over_http_for_cb() {
        let out = classify_services(
            &[svc(
                "commit-boost-1-lighthouse-geth",
                &[("http", "http://x:18550"), ("pbs", "http://x:9999")],
            )],
            no_fallback,
        );
        assert_eq!(out.cb_pbs_urls, vec!["http://x:9999"], "pbs wins over http");
    }

    #[test]
    fn classify_uses_the_fallback_only_when_the_port_was_not_parsed() {
        // A service whose ports the inspect table did not carry: the injected
        // lookup supplies it. This is the ONLY place IO happens in discovery.
        let out = classify_services(&[svc("cl-1-lighthouse-geth", &[])], |svc, port| {
            assert_eq!((svc, port), ("cl-1-lighthouse-geth", "http"));
            Some("http://fallback:1".to_string())
        });
        assert_eq!(out.beacon_urls, vec!["http://fallback:1"]);
    }

    #[test]
    fn classify_skips_a_service_with_no_usable_port() {
        // Must warn and continue, never panic or emit a bogus URL.
        let out = classify_services(&[svc("cl-1-lighthouse-geth", &[])], |_, _| None);
        assert!(out.beacon_urls.is_empty());
    }

    #[test]
    fn classify_cb_metrics_are_optional() {
        // Metrics absent is the DEFAULT devnet shape (matrix checks then SKIP);
        // it must not stop the PBS url from being discovered.
        let out = classify_services(
            &[svc(
                "commit-boost-1-lighthouse-geth",
                &[("http", "http://x:1")],
            )],
            |_, _| None,
        );
        assert_eq!(out.cb_pbs_urls.len(), 1);
        assert!(out.cb_metrics_urls.is_empty());
    }

    #[test]
    fn classify_handles_multi_relay_and_multi_cb() {
        let out = classify_services(
            &[
                svc("helix-relay-2", &[("endpoint", "http://h2:1")]),
                svc("helix-relay-3", &[("endpoint", "http://h3:1")]),
                svc("commit-boost-1-lighthouse-geth", &[("http", "http://c1:1")]),
                svc("commit-boost-2-lighthouse-geth", &[("http", "http://c2:1")]),
            ],
            no_fallback,
        );
        assert_eq!(out.relay_urls.len(), 2, "2-helix topology");
        assert_eq!(out.cb_service_names.len(), 2);
    }

    #[test]
    fn test_relay_identity() {
        assert_eq!(relay_identity("helix-relay").as_deref(), Some("helix"));
        assert_eq!(relay_identity("Helix-Relay").as_deref(), Some("helix"));
        assert_eq!(
            relay_identity("mev-relay-api").as_deref(),
            Some("flashbots")
        );
        assert_eq!(relay_identity("mev-rs-relay").as_deref(), Some("mev-rs"));
        // Non-relay services: function should not be called for these
        // in practice (is_relay_api_service filters them), but they
        // won't match anything meaningful.
        assert_eq!(relay_identity("prometheus"), None);
    }
    use super::*;

    #[test]
    fn test_is_relay_api_service() {
        // Relay API services — should match
        assert!(is_relay_api_service("mev-relay-api"));
        assert!(is_relay_api_service("helix-relay"));
        assert!(is_relay_api_service("mev-rs-relay"));
        assert!(is_relay_api_service("mev-relay-0-api"));
        assert!(is_relay_api_service("Helix-Relay")); // case-insensitive

        // Non-API supporting services — should not match
        assert!(!is_relay_api_service("mev-relay-postgres"));
        assert!(!is_relay_api_service("mev-relay-redis"));
        assert!(!is_relay_api_service("mev-relay-website"));
        assert!(!is_relay_api_service("mev-relay-housekeeper"));
        assert!(!is_relay_api_service("helix-relay-postgres"));

        // Unrelated services — should not match
        assert!(!is_relay_api_service("cl-1-lighthouse-geth"));
        assert!(!is_relay_api_service("prometheus"));
        assert!(!is_relay_api_service("dora"));
        assert!(!is_relay_api_service("commit-boost-001"));
    }

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

    // Parse the real `kurtosis enclave inspect` fixture (a prime format-drift
    // trap). Asserts the User Services table is read into service names + ports,
    // that the header/separator/Files-Artifacts noise is skipped, and that a
    // `<none>` ports column yields no parsed ports.
    //
    // Note: the shared fixture (also consumed by `sim triage`) contains only a
    // beacon (cl-*) and a relay (mev-relay-*) service; it has no commit-boost
    // service, so cb-name/port extraction isn't exercised here (extending the
    // fixture would break the triage test's `len == 2` assertion).
    #[test]
    fn test_parse_services_from_fixture() {
        const INSPECT: &str = include_str!("../tests/fixtures/enclave_inspect.txt");
        let services = parse_services(INSPECT);

        // Exactly the two User Services rows (the Files Artifacts table and all
        // header/separator lines are skipped).
        assert_eq!(
            services.len(),
            2,
            "expected 2 user services, got {:?}",
            services.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        // Beacon service: name + a parsed `http` port URL.
        let beacon = services
            .iter()
            .find(|s| s.name == "cl-1-lighthouse-geth")
            .expect("beacon service parsed");
        let (_, http_url) = beacon
            .ports
            .iter()
            .find(|(name, _)| name == "http")
            .expect("beacon http port parsed");
        assert!(
            http_url.contains("127.0.0.1:32811"),
            "unexpected http url: {http_url}"
        );

        // Relay service: name is extracted even though its ports column is
        // `<none>` (a stopped service), which parses to zero ports.
        let relay = services
            .iter()
            .find(|s| s.name == "mev-relay-helix")
            .expect("relay service parsed");
        assert!(
            relay.ports.is_empty(),
            "a `<none>` ports column must parse to no ports, got {:?}",
            relay.ports
        );
    }
}
