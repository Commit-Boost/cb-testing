//! MUX routing verification check.
//!
//! Extracts `[[mux]]` sections from a Commit-Boost config TOML file.
//! If the config contains mux rules, automatically verifies that every
//! delivered payload conforms — zero cross-contamination between relays.
//! If no mux sections are found, the check is skipped.

use std::collections::{HashMap, HashSet};

use crate::checks::{CheckResult, CheckStatus};
use crate::relay::RelayClient;

/// A single parsed mux entry with the relay index resolved from the template.
pub struct MuxEntry {
    pub id: String,
    pub relay_index: usize,
    pub validator_pubkeys: Vec<String>,
}

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

/// Extract the `commit_boost_config` field from a Kurtosis YAML config.
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

/// Quick inspection: does the raw text look like a TOML with `[[mux]]`?
fn has_mux_sections(text: &str) -> bool {
    text.lines().any(|l| l.trim() == "[[mux]]" || l.trim() == "[[mux]]")
}

/// Parse `[[mux]]` sections from a Commit-Boost config TOML template.
///
/// The template may contain Go template expressions like `{{ index .Relays 0 }}`
/// in string values. This works around that by doing a line-by-line parse
/// rather than using a TOML parser (which would choke on template syntax).
fn parse_mux_from_toml_template(template: &str) -> eyre::Result<Option<Vec<MuxEntry>>> {
    if !has_mux_sections(template) {
        return Ok(None);
    }

    let mut entries: Vec<MuxEntry> = Vec::new();
    let mut lines = template.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();

        // Look for [[mux]] section start (NOT [[mux.relays]] sub-sections)
        if trimmed != "[[mux]]" {
            continue;
        }

        // Parse this mux section
        let entry = match parse_one_mux_section(&mut lines) {
            Ok(Some(e)) => e,
            Ok(None) => continue, // parse_one_mux_section advanced but returned nothing
            Err(e) => return Err(eyre::eyre!("Failed to parse mux section: {e}")),
        };

        entries.push(entry);
    }

    if entries.is_empty() {
        return Ok(None);
    }

    Ok(Some(entries))
}

/// Parse a single `[[mux]]` section (until next `[[` header or EOF).
///
/// Uses `peek()` to avoid consuming the next `[[mux]]` header — the caller's
/// main loop needs to see that line to start the next iteration.
fn parse_one_mux_section<'a>(
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
) -> eyre::Result<Option<MuxEntry>> {
    let mut id: Option<String> = None;
    let mut pubkeys: Option<Vec<String>> = None;
    let mut relay_index: Option<usize> = None;

    loop {
        // Peek at what's next without consuming it
        let is_section_header = lines
            .peek()
            .map(|l| l.trim().starts_with("[["))
            .unwrap_or(false);

        if is_section_header {
            let header = lines.peek().unwrap().trim().to_string();
            if header.starts_with("[[mux.relays]]") {
                // Consume the [[mux.relays]] header and parse its body
                let _ = lines.next();
                let idx = parse_mux_relay_body(lines)?;
                if relay_index.is_none() {
                    relay_index = idx;
                }
                continue;
            }
            // Any other [[ section — stop without consuming.
            // The caller's main loop will pick up the header.
            break;
        }

        let Some(line) = lines.next() else {
            break;
        };
        let trimmed = line.trim();

        // Skip comments and blank lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // key = "value"
        if let Some((key, raw_val)) = parse_key_value(trimmed) {
            match key {
                "id" => {
                    id = Some(raw_val.trim_matches('"').to_string());
                }
                "validator_pubkeys" => {
                    pubkeys = Some(parse_pubkey_array(raw_val, lines)?);
                }
                _ => {} // Ignore other keys
            }
        }
    }

    let id = id.ok_or_else(|| eyre::eyre!("[[mux]] section missing 'id' field"))?;
    let pubkeys = pubkeys
        .ok_or_else(|| eyre::eyre!("[[mux]] section '{id}' missing 'validator_pubkeys'"))?;
    let relay_index = relay_index.ok_or_else(|| {
        eyre::eyre!("[[mux]] section '{id}' missing [[mux.relays]] with url template")
    })?;

    Ok(Some(MuxEntry {
        id,
        relay_index,
        validator_pubkeys: pubkeys,
    }))
}

/// Parse the body of a [[mux.relays]] sub-section (after its header was
/// consumed) to extract the relay index from `url = "{{ index .Relays N }}"`.
///
/// Stops at the next `[[` section header without consuming it.
fn parse_mux_relay_body(
    lines: &mut std::iter::Peekable<std::str::Lines>,
) -> eyre::Result<Option<usize>> {
    loop {
        // Stop at next section header without consuming
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
                if let Some(idx) = parse_relay_index_from_template(val) {
                    return Ok(Some(idx));
                }
                return Ok(None);
            }
        }
    }

    Ok(None)
}

/// Extract the relay index from a Go template expression like `{{ index .Relays 0 }}`.
fn parse_relay_index_from_template(val: &str) -> Option<usize> {
    let val = val.trim();
    // Match: {{ index .Relays N }} or {{ index .Relays N}}
    // The N is a decimal integer
    let stripped = val
        .trim_start_matches("{{")
        .trim_end_matches("}}")
        .trim();
    // Now stripped should be something like: index .Relays 0
    let parts: Vec<&str> = stripped.split_whitespace().collect();
    // Expected: ["index", ".Relays", "N"]
    if parts.len() >= 3 && parts[0] == "index" && parts[1] == ".Relays" {
        parts[2].parse::<usize>().ok()
    } else {
        None
    }
}

/// Parse a simple key = "value" pair.
fn parse_key_value(s: &str) -> Option<(&str, &str)> {
    let eq_pos = s.find('=')?;
    let key = s[..eq_pos].trim();
    let raw_val = s[eq_pos + 1..].trim();
    Some((key, raw_val))
}

/// Parse a TOML-style array literal: `["0x...", "0x...", ...]`
///
/// Handles both single-line and multi-line formats.
/// `rest` is what follows the `=` sign. If it starts with `[`, the array
/// is inline. If not, the array starts on the next line(s).
fn parse_pubkey_array(
    rest: &str,
    lines: &mut std::iter::Peekable<std::str::Lines>,
) -> eyre::Result<Vec<String>> {
    let mut accum = rest.to_string();

    // Collect lines until we find the closing `]`.
    // Works for both single-line (`rest` already contains `]`) and
    // multi-line (rest is just `[`, or rest is the start up to `= [`).
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

    // Now parse the array from accum
    let raw = accum.trim();

    // Find the opening and closing brackets
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
        if item.is_empty() {
            continue;
        }
        // Strip Go template expressions if any leaked in
        if item.contains("{{") || item.contains("}}") {
            continue;
        }
        pubkeys.push(item.to_string());
    }

    Ok(pubkeys)
}

/// Normalize a public key string: strip `0x` prefix, lowercase.
fn normalize_pubkey(pk: &str) -> String {
    pk.trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_lowercase()
}

/// Run the mux routing check against the relay data.
///
/// Returns SKIP if entries is empty (no mux config).
/// Returns PASS/WARN/FAIL based on cross-contamination findings.
pub async fn check_mux_routing(
    relays: &[RelayClient],
    entries: &[MuxEntry],
    start_slot: u64,
    end_slot: u64,
) -> CheckResult {
    if entries.is_empty() {
        return CheckResult::skip(
            "mux.routing",
            1,
            "No [[mux]] sections in CB config — nothing to verify",
        );
    }

    if entries.len() > relays.len() {
        return CheckResult::fail(
            "mux.routing",
            1,
            format!(
                "Mux config has {} entries but only {} relay(s) discovered",
                entries.len(),
                relays.len()
            ),
        );
    }

    // Validate relay indices are in range
    for entry in entries {
        if entry.relay_index >= relays.len() {
            return CheckResult::fail(
                "mux.routing",
                1,
                format!(
                    "Mux '{}' references relay_index {} but only {} relay(s) discovered",
                    entry.id,
                    entry.relay_index,
                    relays.len()
                ),
            );
        }
    }

    // Build: normalized_pubkey -> expected_relay_index
    let mut expected_relay: HashMap<String, usize> = HashMap::new();
    for entry in entries {
        for pk in &entry.validator_pubkeys {
            let nk = normalize_pubkey(pk);
            expected_relay.insert(nk, entry.relay_index);
        }
    }
    let total_mapped_pubkeys = expected_relay.len();

    // Query each relay's delivered payloads in the observation window
    let mut violations: Vec<serde_json::Value> = Vec::new();
    let mut seen_pubkeys: HashMap<String, HashSet<u64>> = HashMap::new();

    for (relay_idx, relay) in relays.iter().enumerate() {
        let Ok(entries) = relay.get_payloads_delivered(start_slot, end_slot).await else {
            tracing::warn!(
                "mux check: relay[{relay_idx}] {} payload query failed",
                relay.base_url()
            );
            continue;
        };

        tracing::info!(
            "mux check: relay[{relay_idx}] {} returned {} delivered payload(s)",
            relay.base_url(),
            entries.len()
        );

        for payload in &entries {
            let pk_hex = payload.proposer_pubkey.to_string();
            let pk_norm = normalize_pubkey(&pk_hex);

            if let Some(&expected_idx) = expected_relay.get(&pk_norm) {
                if expected_idx != relay_idx {
                    violations.push(serde_json::json!({
                        "slot": payload.slot,
                        "proposer_pubkey": format!("0x{pk_norm}"),
                        "relay_actual": relay_idx,
                        "relay_expected": expected_idx,
                    }));
                }

                seen_pubkeys
                    .entry(pk_norm)
                    .or_default()
                    .insert(payload.slot);
            }
        }
    }

    // Mapped pubkeys that never appeared in any relay's deliveries
    let mut unseen: Vec<String> = Vec::new();
    for pk_norm in expected_relay.keys() {
        if !seen_pubkeys.contains_key(pk_norm) {
            unseen.push(format!("0x{pk_norm}"));
        }
    }

    let data = serde_json::json!({
        "total_mapped_pubkeys": total_mapped_pubkeys,
        "violations": violations,
        "violation_count": violations.len(),
        "unseen_pubkeys": unseen,
        "unseen_count": unseen.len(),
        "total_deliveries_checked": seen_pubkeys.values().map(|s| s.len()).sum::<usize>(),
    });

    let mux_ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let mux_detail = format!("muxes=[{}]", mux_ids.join(", "));

    if !violations.is_empty() {
        CheckResult::fail(
            "mux.routing",
            1,
            format!(
                "{} mux routing violation(s): pubkey(s) delivered to the wrong relay. {}",
                violations.len(),
                mux_detail,
            ),
        )
        .with_data(data)
    } else if !unseen.is_empty() {
        CheckResult::warn(
            "mux.routing",
            1,
            format!(
                "No routing violations ✓. {} mux-mapped pubkey(s) had zero deliveries \
                 in the observation window (they may not have proposed). {}",
                unseen.len(),
                mux_detail,
            ),
        )
        .with_data(data)
    } else {
        CheckResult::pass(
            "mux.routing",
            1,
            format!(
                "All {} mux-mapped pubkey(s) delivered exclusively to their assigned relays. \
                 Zero routing violations. {}",
                total_mapped_pubkeys,
                mux_detail,
            ),
        )
        .with_data(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            parse_relay_index_from_template("{{ index .Relays 42 }}"),
            Some(42)
        );
        assert_eq!(
            parse_relay_index_from_template("{{ index .Relays 0}}"),
            Some(0)
        );
        // Not a template
        assert_eq!(
            parse_relay_index_from_template("http://relay:18550"),
            None
        );
        // Range variable reference
        assert_eq!(parse_relay_index_from_template("{{ $relay }}"), None);
    }

    #[test]
    fn test_has_mux_sections() {
        assert!(has_mux_sections("[[mux]]\nid = 'foo'"));
        assert!(!has_mux_sections("[[relays]]\nid = 'foo'"));
    }

    #[test]
    fn test_normalize_pubkey() {
        assert_eq!(
            normalize_pubkey("0xABC123"),
            "abc123"
        );
        assert_eq!(
            normalize_pubkey("abc123"),
            "abc123"
        );
        assert_eq!(
            normalize_pubkey("0XABC123"),
            "abc123"
        );
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
        assert_eq!(entries[0].relay_index, 0);
        assert_eq!(entries[0].validator_pubkeys.len(), 2);
        assert_eq!(entries[1].id, "node_1_to_flashbots");
        assert_eq!(entries[1].relay_index, 1);
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
}
