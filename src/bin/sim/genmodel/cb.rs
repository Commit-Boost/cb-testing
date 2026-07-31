//! The commit-boost TOML block — a verbatim port of the Python
//! `build_cb_toml_basic` / `build_cb_toml_mux`
//! (`scripts/generate_kurtosis_configs.py`).
//!
//! The `chain = {...}`, `port = {{ .Port }}`, and
//! `{{ range $index, $relay := .Relays }} … {{- end }}` are runtime holes filled
//! by the ethereum-package at launch — literal text here. Generate-time knobs
//! (timeouts, extra `[pbs]` lines, per-relay lines) are injected by plain string
//! building — NOT serde — so there is no quoting/sentinel hazard.

/// Generate-time knobs for the basic CB template (`build_cb_toml_basic`).
#[derive(Debug, Clone)]
pub struct CbParams {
    pub timeout_get_header_ms: u32,
    pub timeout_get_payload_ms: u32,
    /// Extra `[pbs]` lines, inserted after `port` and before the timeouts.
    pub extra_pbs_lines: Vec<String>,
    /// Extra lines appended inside the `{{ range }}` relay loop (per relay).
    pub per_relay_lines: Vec<String>,
}

impl CbParams {
    /// The default basic knobs (950 / 4000, no extra lines) — cb-basic and
    /// cb-multiple-relays.
    pub fn basic() -> Self {
        Self {
            timeout_get_header_ms: 950,
            timeout_get_payload_ms: 4000,
            extra_pbs_lines: Vec::new(),
            per_relay_lines: Vec::new(),
        }
    }
}

/// Reproduce `build_cb_toml_basic`: base template + Rust-side injection of the
/// timeouts, `extra_pbs_lines` (after `port`, before the timeouts) and
/// `per_relay_lines` (inside the range loop).
pub fn cb_toml(p: &CbParams) -> String {
    let mut lines: Vec<String> = vec![
        r#"chain = { genesis_time_secs = {{ .Timestamp }}, path = "{{ .Network }}" }"#.to_string(),
        String::new(),
        "[pbs]".to_string(),
        r#"host = "0.0.0.0""#.to_string(),
        "port = {{ .Port }}".to_string(),
        format!("timeout_get_header_ms = {}", p.timeout_get_header_ms),
        format!("timeout_get_payload_ms = {}", p.timeout_get_payload_ms),
        "late_in_slot_time_ms = 2000".to_string(),
    ];

    // Insert after port (idx 4), before the timeouts (idx 5) — matches Python.
    for (offset, line) in p.extra_pbs_lines.iter().enumerate() {
        lines.insert(5 + offset, line.clone());
    }

    lines.push(String::new());
    lines.push(String::new());
    lines.push("[metrics]".to_string());
    lines.push("enabled = true".to_string());
    lines.push(r#"host = "0.0.0.0""#.to_string());
    lines.push("start_port = 9090".to_string());
    lines.push(String::new());
    lines.push("{{ range $index, $relay := .Relays }}".to_string());
    lines.push("[[relays]]".to_string());
    lines.push(r#"id = "mev_relay_{{$index}}""#.to_string());
    lines.push(r#"url = "{{ $relay }}""#.to_string());

    for line in &p.per_relay_lines {
        lines.push(line.clone());
    }

    lines.push("{{- end }}".to_string());
    lines.push(String::new());
    lines.push("[logs.stdout]".to_string());
    lines.push(r#"level = "debug""#.to_string());
    lines.push(String::new());
    lines.push("[logs.file]".to_string());
    lines.push("enabled = false".to_string());

    lines.join("\n")
}

/// Format a pubkey list as a multiline literal with 4-space entry indentation,
/// matching Python `format_pubkey_list`. Inside the 4-space-indented block
/// scalar the entries land at 8 spaces total.
fn format_pubkey_list(pubkeys: &[String]) -> String {
    let mut lines = vec!["[".to_string()];
    let last = pubkeys.len().saturating_sub(1);
    for (i, pk) in pubkeys.iter().enumerate() {
        let comma = if i == last { "" } else { "," };
        lines.push(format!("    \"{pk}\"{comma}"));
    }
    lines.push("]".to_string());
    lines.join("\n")
}

/// Reproduce `build_cb_toml_mux`: the range loop precedes `[metrics]`, and two
/// `[[mux]]` blocks (per-node `validator_pubkeys` + a `[[mux.relays]]`) sit
/// between `[metrics]` and `[logs]`.
pub fn cb_toml_mux(pubkeys_node0: &[String], pubkeys_node1: &[String]) -> String {
    let node0_list = format_pubkey_list(pubkeys_node0);
    let node1_list = format_pubkey_list(pubkeys_node1);

    let lines: Vec<String> = vec![
        r#"chain = { genesis_time_secs = {{ .Timestamp }}, path = "{{ .Network }}" }"#.to_string(),
        String::new(),
        "[pbs]".to_string(),
        r#"host = "0.0.0.0""#.to_string(),
        "port = {{ .Port }}".to_string(),
        "timeout_get_header_ms = 950".to_string(),
        "timeout_get_payload_ms = 4000".to_string(),
        "late_in_slot_time_ms = 2000".to_string(),
        String::new(),
        "{{ range $index, $relay := .Relays }}".to_string(),
        "[[relays]]".to_string(),
        r#"id = "mev_relay_{{$index}}""#.to_string(),
        r#"url = "{{ $relay }}""#.to_string(),
        "{{- end }}".to_string(),
        String::new(),
        "[metrics]".to_string(),
        "enabled = true".to_string(),
        r#"host = "0.0.0.0""#.to_string(),
        "start_port = 9090".to_string(),
        String::new(),
        "[[mux]]".to_string(),
        r#"id = "node_0_to_helix""#.to_string(),
        format!("validator_pubkeys = {node0_list}"),
        "timeout_get_header_ms = 900".to_string(),
        "[[mux.relays]]".to_string(),
        r#"id = "mux_helix""#.to_string(),
        r#"url = "{{ index .Relays 0 }}""#.to_string(),
        String::new(),
        "[[mux]]".to_string(),
        r#"id = "node_1_to_helix""#.to_string(),
        format!("validator_pubkeys = {node1_list}"),
        "timeout_get_header_ms = 900".to_string(),
        "[[mux.relays]]".to_string(),
        r#"id = "mux_helix_1""#.to_string(),
        r#"url = "{{ index .Relays 1 }}""#.to_string(),
        String::new(),
        "[logs.stdout]".to_string(),
        r#"level = "debug""#.to_string(),
        String::new(),
        "[logs.file]".to_string(),
        "enabled = false".to_string(),
    ];

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genmodel::{extract_block_scalar, golden};

    #[test]
    fn basic_cb_matches_golden_block() {
        let block = extract_block_scalar(golden("cb-basic"), "commit_boost_config");
        assert_eq!(cb_toml(&CbParams::basic()), block);
    }

    #[test]
    fn timing_games_cb_matches_golden_block() {
        // 400/2000 timeouts + 3 per-relay lines INSIDE the range loop.
        let params = CbParams {
            timeout_get_header_ms: 400,
            timeout_get_payload_ms: 2000,
            extra_pbs_lines: Vec::new(),
            per_relay_lines: vec![
                "enable_timing_games = true".to_string(),
                "target_first_request_ms = 100".to_string(),
                "frequency_get_header_ms = 200".to_string(),
            ],
        };
        let block = extract_block_scalar(golden("cb-timing-games"), "commit_boost_config");
        assert_eq!(cb_toml(&params), block);
    }

    #[test]
    fn skip_sigverify_cb_matches_golden_block() {
        let params = CbParams {
            extra_pbs_lines: vec!["skip_sigverify = true".to_string()],
            ..CbParams::basic()
        };
        let block = extract_block_scalar(golden("cb-skip-sigverify"), "commit_boost_config");
        assert_eq!(cb_toml(&params), block);
    }

    #[test]
    fn extra_validation_cb_matches_golden_block() {
        let params = CbParams {
            extra_pbs_lines: vec![
                "extra_validation_enabled = true".to_string(),
                r#"rpc_url = "http://el-1-geth-lighthouse:8545""#.to_string(),
            ],
            ..CbParams::basic()
        };
        let block = extract_block_scalar(golden("cb-extra-validation"), "commit_boost_config");
        assert_eq!(cb_toml(&params), block);
    }

    #[test]
    fn mux_cb_has_the_right_structure() {
        // STRUCTURE-only unit test with 2 synthetic keys/node (NOT 256).
        let node0 = vec!["0xdead".to_string(), "0xbeef".to_string()];
        let node1 = vec!["0xcafe".to_string(), "0xf00d".to_string()];
        let out = cb_toml_mux(&node0, &node1);

        assert_eq!(out.matches("[[mux]]").count(), 2, "two [[mux]] blocks");
        assert_eq!(out.matches("[[mux.relays]]").count(), 2);
        assert!(out.contains(r#"id = "node_0_to_helix""#));
        assert!(out.contains(r#"id = "node_1_to_helix""#));
        // per-node validator_pubkeys lists, entries indented 4 spaces.
        assert!(out.contains("validator_pubkeys = [\n    \"0xdead\",\n    \"0xbeef\"\n]"));
        assert!(out.contains("validator_pubkeys = [\n    \"0xcafe\",\n    \"0xf00d\"\n]"));
        assert!(out.contains(r#"url = "{{ index .Relays 0 }}""#));
        assert!(out.contains(r#"url = "{{ index .Relays 1 }}""#));
        // range loop precedes [metrics] in mux (unlike basic).
        let range_at = out.find("{{ range").unwrap();
        let metrics_at = out.find("[metrics]").unwrap();
        assert!(range_at < metrics_at, "range loop before [metrics] in mux");
    }
}
