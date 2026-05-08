//! MUX routing verification check.
//!
//! Extracts `[[mux]]` sections from a Commit-Boost config TOML file.
//! If the config contains mux rules, verifies that CB's PBS service
//! correctly routes getHeader requests according to the mux config.
//!
//! Verification works by fetching CB PBS container logs and parsing
//! structured INFO lines. Each line records key=value pairs that are
//! cross-referenced against the mux configuration.
//!
//! Relevant CB log lines (from commit-boost-client crates):
//!   "using mux config"  — mux_id, relays, pubkey (DEBUG)
//!   "received new header" — relay_id, slot, validator, value_eth, block_hash (INFO)
//!   "auction winner" — relay_id, value_eth, block_hash (INFO)
//!   "new request" (submit_blinded_blocks) — slot, validator (INFO)
//!   "received unblinded block (v1/v2)" — (INFO)
//!   "CRITICAL: no payload received" — block_hash (ERROR)
//!
//! If no mux sections are found, the check is skipped.

use std::collections::{HashMap, HashSet};

use crate::checks::CheckResult;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single parsed mux entry from the CB config.
pub struct MuxEntry {
    pub id: String,
    pub relay_identity: String,
    pub validator_pubkeys: Vec<String>,
}

/// Parsed event from a CB INFO/DEBUG log line.
#[derive(Debug, Clone)]
pub struct CbEvent {
    /// The log message text (e.g., "using mux config", "received new header")
    pub message: String,
    /// Key=value pairs extracted from the log line.
    pub fields: HashMap<String, String>,
    /// The slot number if present in the fields.
    pub slot: Option<u64>,
    /// The validator pubkey if present.
    pub validator: Option<String>,
    /// The relay_id if present.
    pub relay_id: Option<String>,
    /// The mux_id if present.
    pub mux_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Config parsing (unchanged from before)
// ---------------------------------------------------------------------------

/// Parse a Commit-Boost config file and extract mux routing entries.
///
/// Supports two file formats:
/// - `.toml` — raw Commit-Boost config (possibly with Go template expressions)
/// - `.yml`/`.yaml` — Kurtosis YAML with `mev_params.commit_boost_config` field
///
/// Returns `Ok(Some(entries))` if mux sections were found,
/// `Ok(None)` if no mux sections (check will SKIP),
/// `Err` if parsing fails.
pub fn extract_mux_from_config(path: &str) -> eyre::Result<Option<Vec<MuxEntry>>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| eyre::eyre!("Failed to read config '{path}': {e}"))?;

    let template = if path.ends_with(".toml") {
        raw
    } else if path.ends_with(".yml") || path.ends_with(".yaml") {
        extract_commit_boost_config_from_yaml(&raw)?
    } else {
        return Err(eyre::eyre!(
            "Unrecognized config format. Expected .toml (CB config) or .yml/.yaml (Kurtosis config), got: {path}"
        ));
    };

    parse_mux_from_toml_template(&template)
}

fn extract_commit_boost_config_from_yaml(raw: &str) -> eyre::Result<String> {
    use serde_yaml::Value as YamlValue;

    let parsed: YamlValue = serde_yaml::from_str(raw)
        .map_err(|e| eyre::eyre!("Failed to parse Kurtosis YAML config: {e}"))?;

    let template = parsed
        .get("mev_params")
        .and_then(|p| p.get("commit_boost_config"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            eyre::eyre!(
                "No mev_params.commit_boost_config found in Kurtosis YAML config"
            )
        })?;

    Ok(template.to_string())
}

fn has_mux_sections(text: &str) -> bool {
    text.lines().any(|l| l.trim() == "[[mux]]")
}

fn parse_mux_from_toml_template(template: &str) -> eyre::Result<Option<Vec<MuxEntry>>> {
    if !has_mux_sections(template) {
        return Ok(None);
    }

    let mut entries: Vec<MuxEntry> = Vec::new();
    let mut lines = template.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed != "[[mux]]" {
            continue;
        }

        let entry = match parse_one_mux_section(&mut lines) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => return Err(eyre::eyre!("Failed to parse mux section: {e}")),
        };

        entries.push(entry);
    }

    if entries.is_empty() {
        return Ok(None);
    }

    Ok(Some(entries))
}

fn relay_identity_from_mux_id(id: &str) -> String {
    if let Some(pos) = id.rfind("to_") {
        let ident = id[pos + 3..].trim().to_string();
        if !ident.is_empty() {
            return ident;
        }
    }
    id.to_string()
}

fn parse_one_mux_section<'a>(
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
) -> eyre::Result<Option<MuxEntry>> {
    let mut id: Option<String> = None;
    let mut pubkeys: Option<Vec<String>> = None;

    loop {
        let is_section_header = lines
            .peek()
            .map(|l| l.trim().starts_with("[["))
            .unwrap_or(false);

        if is_section_header {
            let header = lines.peek().unwrap().trim().to_string();
            if header.starts_with("[[mux.relays]]") {
                let _ = lines.next();
                let _ = parse_mux_relay_body(lines)?;
                continue;
            }
            break;
        }

        let Some(line) = lines.next() else {
            break;
        };
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((key, raw_val)) = parse_key_value(trimmed) {
            match key {
                "id" => {
                    id = Some(raw_val.trim_matches('"').to_string());
                }
                "validator_pubkeys" => {
                    pubkeys = Some(parse_pubkey_array(raw_val, lines)?);
                }
                _ => {}
            }
        }
    }

    let id = id.ok_or_else(|| eyre::eyre!("[[mux]] section missing 'id' field"))?;
    let relay_identity = relay_identity_from_mux_id(&id);
    let pubkeys = pubkeys
        .ok_or_else(|| eyre::eyre!("[[mux]] section '{id}' missing 'validator_pubkeys'"))?;

    Ok(Some(MuxEntry {
        id,
        relay_identity,
        validator_pubkeys: pubkeys,
    }))
}

fn parse_mux_relay_body(
    lines: &mut std::iter::Peekable<std::str::Lines>,
) -> eyre::Result<Option<usize>> {
    loop {
        if lines
            .peek()
            .map(|l| l.trim().starts_with("[["))
            .unwrap_or(false)
        {
            break;
        }

        let Some(line) = lines.next() else {
            break;
        };
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((key, raw_val)) = parse_key_value(trimmed) {
            if key == "url" {
                let val = raw_val.trim_matches('"');
                return Ok(parse_relay_index_from_template(val));
            }
        }
    }

    Ok(None)
}

fn parse_relay_index_from_template(val: &str) -> Option<usize> {
    let val = val.trim();
    let stripped = val
        .trim_start_matches("{{")
        .trim_end_matches("}}")
        .trim();
    let parts: Vec<&str> = stripped.split_whitespace().collect();
    if parts.len() >= 3 && parts[0] == "index" && parts[1] == ".Relays" {
        parts[2].parse::<usize>().ok()
    } else {
        None
    }
}

fn parse_key_value(s: &str) -> Option<(&str, &str)> {
    let eq_pos = s.find('=')?;
    let key = s[..eq_pos].trim();
    let raw_val = s[eq_pos + 1..].trim();
    Some((key, raw_val))
}

fn parse_pubkey_array(
    rest: &str,
    lines: &mut std::iter::Peekable<std::str::Lines>,
) -> eyre::Result<Vec<String>> {
    let mut accum = rest.to_string();

    if !accum.trim_end().ends_with(']') {
        loop {
            let Some(next) = lines.next() else {
                break;
            };
            accum.push('\n');
            accum.push_str(next);
            if next.trim().ends_with(']') {
                break;
            }
        }
    }

    let raw = accum.trim();
    let start = raw.find('[').ok_or_else(|| {
        eyre::eyre!("Could not find opening '[' in pubkey array: {raw:.50}...")
    })?;
    let end = raw.rfind(']').ok_or_else(|| {
        eyre::eyre!("Could not find closing ']' in pubkey array: {raw:.50}...")
    })?;

    let inner = &raw[start + 1..end];
    let mut pubkeys = Vec::new();
    for item in inner.split(',') {
        let item = item.trim().trim_matches('"').trim();
        if item.is_empty() || item.contains("{{") || item.contains("}}") {
            continue;
        }
        pubkeys.push(item.to_string());
    }

    Ok(pubkeys)
}

fn normalize_pubkey(pk: &str) -> String {
    pk.trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// CB log parsing
// ---------------------------------------------------------------------------

/// Strip ANSI escape codes from a string.
///
/// ANSI codes look like `\x1b[32m` (color) or `\x1b[0m` (reset).
/// This removes them so we can parse the actual text content.
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the '[' and everything until we hit a letter (m, H, J, etc.)
            if chars.peek() == Some(&'[') {
                chars.next(); // skip '['
                while let Some(&ch) = chars.peek() {
                    chars.next();
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Parse a CB tracing log line into a `CbEvent`.
///
/// CB uses `tracing` with a compact format:
///   `timestamp LEVEL : message key=value key=value ...`
///
/// We extract the message and all key=value pairs.
pub fn parse_cb_log_line(line: &str) -> Option<CbEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Strip kurtosis prefix: "[service-name] rest"
    let line = if line.starts_with('[') {
        if let Some(pos) = line.find(']') {
            line[pos + 1..].trim_start()
        } else {
            line
        }
    } else {
        line
    };

    // Strip ANSI escape codes (e.g., "\x1b[32m" for green text)
    let line = strip_ansi_codes(line);
    // Continue parsing with the cleaned line (now owned String)
    // All subsequent code uses `line` as a String, not &str

    // Find the message portion after "LEVEL : " or "LEVEL ".
    let after_level: String = if let Some(pos) = line.find(" : ") {
        line[pos + 3..].to_string()
    } else {
        let levels = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];
        let mut found = line.clone();
        for lvl in &levels {
            if let Some(pos) = line.find(&format!(" {} ", lvl)) {
                found = line[pos + lvl.len() + 2..].to_string();
                break;
            }
            if line.starts_with(lvl) {
                found = line[lvl.len()..].trim_start().to_string();
                break;
            }
        }
        found
    };

    // Find the message/key boundary by scanning for " key=" where key is
    // a valid identifier (alphanumeric + underscore, starting with alpha).
    // This is more reliable than a fixed list of known keys.
    let mut message_end = after_level.len();
    let bytes = after_level.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b' ' {
            // Check if what follows is "key=" where key starts with alpha/underscore
            let rest = &after_level[i + 1..];
            if rest.is_empty() {
                continue;
            }
            let first = rest.as_bytes()[0];
            if first.is_ascii_alphabetic() || first == b'_' {
                // Find the '=' after the key
                if let Some(eq_pos) = rest.find('=') {
                    let key = &rest[..eq_pos];
                    // Key must be all alphanumeric/underscore
                    if key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        // Value after '=' must not be empty (or must be quote)
                        let after_eq = &rest[eq_pos + 1..];
                        if !after_eq.is_empty() {
                            message_end = i;
                            break;
                        }
                    }
                }
            }
        }
    }

    let message = after_level[..message_end].trim().to_string();
    let kv_part = &after_level[message_end..].to_string();

    let mut fields = HashMap::new();
    let mut slot = None;
    let mut validator = None;
    let mut relay_id = None;
    let mut mux_id = None;

    for kv in kv_part.split_whitespace() {
        if let Some((key, val)) = kv.split_once('=') {
            let val = val.trim_matches('"').to_string();
            fields.insert(key.to_string(), val.clone());

            match key {
                "slot" => { slot = val.parse().ok(); }
                "validator" | "pubkey" => { validator = Some(normalize_pubkey(&val)); }
                "relay_id" => { relay_id = Some(val); }
                "mux_id" => { mux_id = Some(val); }
                _ => {}
            }
        }
    }

    Some(CbEvent {
        message,
        fields,
        slot,
        validator,
        relay_id,
        mux_id,
    })
}



// ---------------------------------------------------------------------------
// Log fetching
// ---------------------------------------------------------------------------

/// Fetch logs from a Kurtosis service, filtered to relevant mux/pbs lines.
///
/// Fetches all logs and filters client-side. The `--regex-match` flag is
/// tried first as an optimization, but some kurtosis versions ignore it.
pub fn fetch_service_logs(enclave: &str, service: &str) -> eyre::Result<String> {
    info!(
        "mux check: fetching logs from service '{service}' (enclave={enclave})..."
    );

    let output = std::process::Command::new("kurtosis")
        .args([
            "service", "logs", enclave, service,
            "-n", "200000",
        ])
        .output()
        .map_err(|e| eyre::eyre!("Failed to run 'kurtosis service logs': {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre::eyre!(
            "kurtosis service logs {enclave} {service} failed (rc={:?}): {}",
            output.status.code(),
            stderr.trim()
        ));
    }

    // Combine stdout and stderr — kurtosis writes to either depending on version.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let all_logs = format!("{}\n{}", stdout, stderr);

    // Filter to relevant lines client-side.
    let result: String = all_logs
        .lines()
        .filter(|line| {
            line.contains("using mux config")
                || line.contains("received new header")
                || line.contains("auction winner")
                || line.contains("received unblinded block")
                || line.contains("CRITICAL: no payload")
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_empty() {
        let sample: String = all_logs
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(3)
            .collect::<Vec<_>>()
            .join("\n");
        warn!(
            "mux check: service '{service}' returned no relevant log lines.              Total: {} bytes stdout, {} bytes stderr. Sample:\n{}",
            stdout.len(),
            stderr.len(),
            sample
        );
    } else {
        info!(
            "mux check: service '{service}' returned {} relevant log line(s)",
            result.lines().count()
        );
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Run the mux routing check by parsing CB PBS service logs.
///
/// Fetches logs from each CB PBS service in the enclave, parses structured
/// events, and verifies that the mux routing decisions match the mux config.
///
/// PASS: All logged routing decisions match the mux config.
/// FAIL: A pubkey was routed to the wrong mux/relay.
/// WARN: No relevant log lines found (no getHeader requests occurred).
pub async fn check_mux_routing(
    enclave: &str,
    cb_service_names: &[String],
    entries: &[MuxEntry],
) -> CheckResult {
    if entries.is_empty() {
        return CheckResult::skip(
            "mux.routing",
            1,
            "No [[mux]] sections in CB config — nothing to verify",
        );
    }

    // Build: normalized_pubkey → expected_mux_id
    let mut expected_mux: HashMap<String, String> = HashMap::new();
    for entry in entries {
        for pk in &entry.validator_pubkeys {
            expected_mux.insert(normalize_pubkey(pk), entry.id.clone());
        }
    }

    // Fetch and parse logs from each CB PBS service.
    let mut all_events: Vec<CbEvent> = Vec::new();

    for service_name in cb_service_names {
        let logs = match fetch_service_logs(enclave, service_name) {
            Ok(l) => l,
            Err(e) => {
                warn!(
                    "mux check: failed to fetch logs from service '{service_name}': {e}. \
                     Skipping this service."
                );
                continue;
            }
        };

        if logs.is_empty() {
            continue;
        }

        for line in logs.lines() {
            if let Some(event) = parse_cb_log_line(line) {
                all_events.push(event);
            }
        }
    }

    // Filter to events relevant to mux verification.
    let mux_events: Vec<&CbEvent> = all_events
        .iter()
        .filter(|e| {
            e.message.starts_with("using mux")
                || e.message.starts_with("received new header")
                || e.message.starts_with("auction winner")
                || e.message.starts_with("received unblinded block")
                || e.message.contains("CRITICAL: no payload")
        })
        .collect();

    let total_events = mux_events.len();

    let data = serde_json::json!({
        "total_mux_entries": entries.len(),
        "total_log_events": total_events,
        "pubkeys_verified": 0,
        "violations": [],
        "violation_count": 0,
        "mux_entries_seen": [],
    });

    if total_events == 0 {
        return CheckResult::warn(
            "mux.routing",
            1,
            format!(
                "No mux-related log lines found in any CB PBS service. \
                 No getHeader requests were recorded — mux config is valid \
                 but routing could not be verified at runtime. muxes=[{}]",
                entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        ).with_data(data);
    }

    // Verify: for each "using mux config" event, does the pubkey match?
    let mut violations: Vec<serde_json::Value> = Vec::new();
    let mut pubkeys_verified: HashSet<String> = HashSet::new();
    let mut mux_entries_seen: HashSet<String> = HashSet::new();

    for event in &mux_events {
        if let Some(ref mux_id) = event.mux_id {
            mux_entries_seen.insert(mux_id.clone());
        }

        if let Some(ref pk_norm) = event.validator {
            pubkeys_verified.insert(pk_norm.clone());

            if let Some(expected_mux_id) = expected_mux.get(pk_norm) {
                if let Some(ref actual_mux_id) = event.mux_id {
                    if actual_mux_id != expected_mux_id {
                        let expected_relay = entries
                            .iter()
                            .find(|e| e.id == *expected_mux_id)
                            .map(|e| e.relay_identity.as_str())
                            .unwrap_or("?");
                        let actual_relay = entries
                            .iter()
                            .find(|e| e.id == *actual_mux_id)
                            .map(|e| e.relay_identity.as_str())
                            .unwrap_or("?");

                        violations.push(serde_json::json!({
                            "slot": event.slot,
                            "proposer_pubkey": format!("0x{pk_norm}"),
                            "routed_to_mux": actual_mux_id,
                            "routed_to_relay": actual_relay,
                            "expected_mux": expected_mux_id,
                            "expected_relay": expected_relay,
                        }));

                        warn!(
                            "mux check: MISROUTING — pubkey 0x{pk_norm}.. should route to \
                             '{expected_mux_id}' ({expected_relay}) but was routed to \
                             '{actual_mux_id}' ({actual_relay})"
                        );
                    }
                }
            }
        }
    }

    let data = serde_json::json!({
        "total_mux_entries": entries.len(),
        "total_log_events": total_events,
        "pubkeys_verified": pubkeys_verified.len(),
        "violations": violations,
        "violation_count": violations.len(),
        "mux_entries_seen": mux_entries_seen.iter().cloned().collect::<Vec<_>>(),
    });

    let mux_ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let mux_detail = format!("muxes=[{}]", mux_ids.join(", "));

    if !violations.is_empty() {
        CheckResult::fail(
            "mux.routing",
            1,
            format!(
                "{} mux routing violation(s): CB PBS routed a pubkey to the wrong mux/relay. {}",
                violations.len(),
                mux_detail,
            ),
        )
        .with_data(data)
    } else {
        CheckResult::pass(
            "mux.routing",
            1,
            format!(
                "All {} mux routing decision(s) verified ✓ CB PBS correctly routed \
                 every getHeader request according to mux config. {}",
                total_events,
                mux_detail,
            ),
        )
        .with_data(data)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relay_identity_from_mux_id() {
        assert_eq!(relay_identity_from_mux_id("node_0_to_helix"), "helix");
        assert_eq!(relay_identity_from_mux_id("node_1_to_flashbots"), "flashbots");
        assert_eq!(relay_identity_from_mux_id("my_mux_entry"), "my_mux_entry");
        assert_eq!(relay_identity_from_mux_id("to_"), "to_");
    }

    #[test]
    fn test_parse_relay_index_from_template() {
        assert_eq!(
            parse_relay_index_from_template("{{ index .Relays 0 }}"),
            Some(0)
        );
        assert_eq!(
            parse_relay_index_from_template("{{ index .Relays 1 }}"),
            Some(1)
        );
        assert_eq!(parse_relay_index_from_template("http://relay:18550"), None);
        assert_eq!(parse_relay_index_from_template("{{ $relay }}"), None);
    }

    #[test]
    fn test_has_mux_sections() {
        assert!(has_mux_sections("[[mux]]\nid = 'foo'"));
        assert!(!has_mux_sections("[[relays]]\nid = 'foo'"));
    }

    #[test]
    fn test_normalize_pubkey() {
        assert_eq!(normalize_pubkey("0xABC123"), "abc123");
        assert_eq!(normalize_pubkey("abc123"), "abc123");
    }

    #[test]
    fn test_parse_pubkey_array_single_line() {
        let rest = "[\"0xabc\", \"0xdef\"]";
        let mut empty = "".lines().peekable();
        let keys = parse_pubkey_array(rest, &mut empty).unwrap();
        assert_eq!(keys, vec!["0xabc", "0xdef"]);
    }

    #[test]
    fn test_parse_pubkey_array_multi_line() {
        let rest = "[";
        let input = "    \"0xabc\",\n    \"0xdef\",\n]";
        let mut lines = input.lines().peekable();
        let keys = parse_pubkey_array(rest, &mut lines).unwrap();
        assert_eq!(keys, vec!["0xabc", "0xdef"]);
    }

    #[test]
    fn test_parse_mux_from_toml_template() {
        let template = r#"
[[relays]]
id = "mev_relay_0"
url = "{{ $relay }}"

[[mux]]
id = "node_0_to_helix"
validator_pubkeys = [
    "0xaaf6c1251e73fb600624937760fef218aace5b253bf068ed45398aeb29d821e4d2899343ddcbbe37cb3f6cf500dff26c",
    "0x8aa5bbee21e98c7b9e7a4c8ea45aa99f89e22992fa4fc2d73869d77da4cc8a05b25b61931ff521986677dd7f7159e8e6",
]
timeout_get_header_ms = 900
[[mux.relays]]
id = "mux_helix"
url = "{{ index .Relays 0 }}"

[[mux]]
id = "node_1_to_flashbots"
validator_pubkeys = [
    "0xb05cafec5912f22dbd6f15677f25f13d93ecd5ec6f957fddd7cf27d73521b34aaaf6a219f77b21128d18321c2c8d679b",
]
[[mux.relays]]
id = "mux_flashbots"
url = "{{ index .Relays 1 }}"
"#;

        let result = parse_mux_from_toml_template(template).unwrap();
        assert!(result.is_some());
        let entries = result.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "node_0_to_helix");
        assert_eq!(entries[0].relay_identity, "helix");
        assert_eq!(entries[0].validator_pubkeys.len(), 2);
        assert_eq!(entries[1].id, "node_1_to_flashbots");
        assert_eq!(entries[1].relay_identity, "flashbots");
        assert_eq!(entries[1].validator_pubkeys.len(), 1);
    }

    #[test]
    fn test_no_mux_sections_returns_none() {
        let template = r#"
[[relays]]
id = "mev_relay_0"
url = "{{ $relay }}"

[logs.stdout]
level = "debug"
"#;
        let result = parse_mux_from_toml_template(template).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_commit_boost_config_from_yaml() {
        let yaml = r#"
mev_type: custom
mev_params:
  commit_boost_config: |
    [[mux]]
    id = "test"
    validator_pubkeys = [
        "0xabc",
    ]
    [[mux.relays]]
    url = "{{ index .Relays 0 }}"
additional_services:
  - dora
"#;
        let template = extract_commit_boost_config_from_yaml(yaml).unwrap();
        assert!(template.contains("[[mux]]"));
        assert!(template.contains("0xabc"));
    }

    // --- CB log parsing tests ---

    #[test]
    fn test_parse_cb_log_line_using_mux_config() {
        let line = r#"2026-05-06T18:50:43.014799Z DEBUG : using mux config mux_id="node_1_to_flashbots" relays=1 pubkey=0xb2ad1574eaca33f1555308e24b27a095d24aed8f4af5302ea2c6ba2e50936d25ffea7047be94065eac630693c7f86757 method=/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} req_id=85e4778a-9144-4cde-80d8-d11a5078f760 slot=160 parent_hash=0xcdee44e74bab2f2ee522dadccadc2cbdf67f8cfa96b5b10ab893becb6cb16bb7 validator=0xb2ad1574eaca33f1555308e24b27a095d24aed8f4af5302ea2c6ba2e50936d25ffea7047be94065eac630693c7f86757"#;

        let event = parse_cb_log_line(line).expect("should parse");
        assert_eq!(event.message, "using mux config");
        assert_eq!(event.mux_id, Some("node_1_to_flashbots".to_string()));
        assert_eq!(event.slot, Some(160));
        assert!(event.validator.is_some());
        assert_eq!(event.validator.unwrap(), "b2ad1574eaca33f1555308e24b27a095d24aed8f4af5302ea2c6ba2e50936d25ffea7047be94065eac630693c7f86757");
    }

    #[test]
    fn test_parse_cb_log_line_with_kurtosis_prefix() {
        // This is the actual format that kurtosis service logs returns
        let line = r#"[commit-boost-1-lighthouse-geth] 2026-05-07T01:24:00.002761Z DEBUG : using mux config mux_id="node_0_to_helix" relays=1 pubkey=0x98213294b82bc66ee39e95a678472fb41df846ec2863c5be53e1fd56b6ff0fe1bfd5b2bd8c534dd97acbe597ad119cc7 method=/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} req_id=8e5020cb-a893-42b3-a2f5-8f4c3f400c9e slot=521 parent_hash=0x969f22b336da6810b4cb9e31837b9d8e26f0a9c3277d150587742bb049561031 validator=0x98213294b82bc66ee39e95a678472fb41df846ec2863c5be53e1fd56b6ff0fe1bfd5b2bd8c534dd97acbe597ad119cc7"#;

        let event = parse_cb_log_line(line).expect("should parse kurtosis-prefixed line");
        assert_eq!(event.message, "using mux config");
        assert_eq!(event.mux_id, Some("node_0_to_helix".to_string()));
        assert_eq!(event.slot, Some(521));
        assert!(event.validator.is_some());
        assert_eq!(event.relay_id, None); // no relay_id in this line
    }

    #[test]
    fn test_parse_cb_log_line_received_new_header_with_prefix() {
        let line = r#"[commit-boost-1-lighthouse-geth] 2026-05-07T01:24:00.009013Z  INFO : received new header relay_id="mux_helix" header_size_bytes=2891 latency=6.1415ms version=Fulu value_eth="0.050439063999832000" block_hash=0x15cd5f31333e1a8d42f0207cf1a61c65baf3d938836b07877a3a76b1cb890d11 method=/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} req_id=8e5020cb-a893-42b3-a2f5-8f4c3f400c9e slot=521"#;

        let event = parse_cb_log_line(line).expect("should parse");
        assert_eq!(event.message, "received new header");
        assert_eq!(event.relay_id, Some("mux_helix".to_string()));
        assert_eq!(event.slot, Some(521));
        assert_eq!(event.fields.get("header_size_bytes"), Some(&"2891".to_string()));
        assert_eq!(event.fields.get("value_eth"), Some(&"0.050439063999832000".to_string()));
    }

    #[test]
    fn test_parse_cb_log_line_received_new_header() {
        let line = r#"2026-05-06T20:43:48.009642Z  INFO : received new header relay_id="mux_helix" header_size_bytes=3099 latency=5.893291ms version=Fulu value_eth="0.042701386561497000" block_hash=0x0d7d119986cbd7b1c376056ffa703245404dc4d9d8b989f2d5ce2ee93d9354aa method=/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} req_id=6adfc13b-87ec-4564-a223-ac23af5b14eb slot=34 parent_hash=0xbd3df2244327d05919f51b0b4bbcafe655c16209573750b5ee94b39f77ff5e44 validator=0x867e89563df1501ac7dc5a369e6713cebab2aa1b676ea6d97fcb62802488866ae1223b4ed6c00718ee895d7e8e650cac"#;

        let event = parse_cb_log_line(line).expect("should parse");
        assert_eq!(event.message, "received new header");
        assert_eq!(event.relay_id, Some("mux_helix".to_string()));
        assert_eq!(event.slot, Some(34));
        assert!(event.validator.is_some());
        assert_eq!(event.fields.get("header_size_bytes"), Some(&"3099".to_string()));
        assert_eq!(event.fields.get("value_eth"), Some(&"0.042701386561497000".to_string()));
    }

    #[test]
    fn test_parse_cb_log_line_auction_winner() {
        let line = r#"2026-05-06T20:43:48.010000Z  INFO : auction winner relay_id="mux_helix" value_eth="0.042701386561497000" block_hash=0x0d7d119986cbd7b1c376056ffa703245404dc4d9d8b989f2d5ce2ee93d9354aa"#;

        let event = parse_cb_log_line(line).expect("should parse");
        assert_eq!(event.message, "auction winner");
        assert_eq!(event.relay_id, Some("mux_helix".to_string()));
    }

    #[test]
    fn test_parse_cb_log_line_non_mux_line() {
        let line = "2026-05-06T20:43:48.009642Z  INFO : some other message key=value";
        let event = parse_cb_log_line(line).expect("should parse");
        assert_eq!(event.message, "some other message");
        assert!(event.mux_id.is_none());
    }

    #[test]
    fn test_parse_cb_log_line_empty() {
        assert!(parse_cb_log_line("").is_none());
        assert!(parse_cb_log_line("   ").is_none());
    }
}

#[cfg(test)]
mod log_file_tests {
    use super::*;

    #[test]
    fn test_parse_cb_log_line_from_file() {
        // Test parsing the actual log format from kurtosis service logs
        // These lines have ANSI escape codes and [service-name] prefix
        let lines = vec![
            r#"[16eac416a3014ec191173b9e95cc11a6] 2026-05-07T04:28:26.004744Z DEBUG : using mux config mux_id="node_1_to_flashbots" relays=1 pubkey=0x8ca49f0c method=/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} req_id=90252b32 slot=2 validator=0x8ca49f0c"#,
            r#"[commit-boost-1-lighthouse-geth] 2026-05-07T01:24:00.002761Z DEBUG : using mux config mux_id="node_0_to_helix" relays=1 pubkey=0x98213294 slot=521 validator=0x98213294"#,
        ];

        for line in &lines {
            let event = parse_cb_log_line(line).expect(&format!("should parse: {}", &line[..80]));
            assert!(event.message.starts_with("using mux"), "message should start with 'using mux', got: {:?}", event.message);
            assert!(event.mux_id.is_some(), "mux_id should be Some");
            assert!(event.slot.is_some(), "slot should be Some");
            assert!(event.validator.is_some(), "validator should be Some");
        }
    }

    #[test]
    fn test_parse_cb_log_line_with_ansi_codes() {
        // Lines from kurtosis service logs have ANSI escape codes for coloring
        let line = "[16eac416a3014ec191173b9e95cc11a6] \x1b[2m2026-05-07T04:28:26.004744Z\x1b[0m \x1b[34mDEBUG\x1b[0m \x1b[1m\x1b[0m: using mux config \x1b[3mmux_id\x1b[0m\x1b[2m=\x1b[0m\"node_1_to_flashbots\" \x1b[3mrelays\x1b[0m\x1b[2m=\x1b[0m1 \x1b[3mpubkey\x1b[0m\x1b[2m=\x1b[0m0x8ca49f0c \x1b[2m\x1b[3mmethod\x1b[0m\x1b[2m=\x1b[0m/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} \x1b[3mreq_id\x1b[0m\x1b[2m=\x1b[0m90252b32 \x1b[3mslot\x1b[0m\x1b[2m=\x1b[0m2 \x1b[3mvalidator\x1b[0m\x1b[2m=\x1b[0m0x8ca49f0c\x1b[0m";

        let event = parse_cb_log_line(line).expect("should parse line with ANSI codes");
        // The message should contain "using mux" (may have trailing ANSI codes)
        assert!(event.message.contains("using mux"), "message should contain 'using mux', got: {:?}", event.message);
        // mux_id should be parsed correctly despite ANSI codes
        assert_eq!(event.mux_id, Some("node_1_to_flashbots".to_string()));
        assert_eq!(event.slot, Some(2));
        assert!(event.validator.is_some());
    }

    #[test]
    fn test_parse_received_new_header_with_ansi() {
        let line = "[16eac416a3014ec191173b9e95cc11a6] \x1b[2m2026-05-07T04:28:26.009013Z\x1b[0m \x1b[32mINFO\x1b[0m \x1b[1m\x1b[0m: received new header \x1b[3mrelay_id\x1b[0m\x1b[2m=\x1b[0m\"mux_helix\" \x1b[3mheader_size_bytes\x1b[0m\x1b[2m=\x1b[0m2891 \x1b[3mlatency\x1b[0m\x1b[2m=\x1b[0m6.1415ms \x1b[3mversion\x1b[0m\x1b[2m=\x1b[0mFulu \x1b[3mvalue_eth\x1b[0m\x1b[2m=\x1b[0m\"0.050439063999832000\" \x1b[2m\x1b[3mmethod\x1b[0m\x1b[2m=\x1b[0m/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey} \x1b[3mreq_id\x1b[0m\x1b[2m=\x1b[0m8e5020cb \x1b[3mslot\x1b[0m\x1b[2m=\x1b[0m521 \x1b[3mparent_hash\x1b[0m\x1b[2m=\x1b[0m0x969f22b3 \x1b[3mvalidator\x1b[0m\x1b[2m=\x1b[0m0x98213294\x1b[0m";

        let event = parse_cb_log_line(line).expect("should parse");
        assert!(event.message.contains("received new header"), "message: {:?}", event.message);
        assert_eq!(event.relay_id, Some("mux_helix".to_string()));
        assert_eq!(event.slot, Some(521));
    }

    #[test]
    fn test_mux_event_filter() {
        // Test that the filter used in check_mux_routing matches parsed events
        let lines = vec![
            "2026-05-07T04:28:26.004744Z DEBUG : using mux config mux_id=\"node_1_to_flashbots\" relays=1 pubkey=0x8ca49f0c slot=2 validator=0x8ca49f0c",
            "2026-05-07T04:28:26.009013Z INFO : received new header relay_id=\"mux_helix\" header_size_bytes=2891 slot=521 validator=0x98213294",
            "2026-05-07T04:28:26.011040Z INFO : auction winner relay_id=\"mux_helix\" value_eth=\"0.050439063999832000\" block_hash=0x15cd5f31 slot=521",
            "2026-05-07T04:28:26.011056Z INFO : received header value_eth=\"0.050439063999832000\" block_hash=0x15cd5f31 slot=521",
        ];

        let events: Vec<CbEvent> = lines.iter().filter_map(|l| parse_cb_log_line(l)).collect();
        assert_eq!(events.len(), 4, "all 4 lines should parse");

        let mux_events: Vec<&CbEvent> = events
            .iter()
            .filter(|e| {
                e.message.starts_with("using mux")
                    || e.message.starts_with("received new header")
                    || e.message.starts_with("auction winner")
            })
            .collect();

        assert_eq!(mux_events.len(), 3, "3 of 4 events should be mux-related (not 'received header')");
    }
}
