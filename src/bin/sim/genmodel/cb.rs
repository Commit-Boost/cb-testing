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
    /// When `Some`, the `{{ range }}` relay loop is REPLACED by a single
    /// literal `[[relays]]` block with this exact url — the fault-injection
    /// seam for the sigverify differential (a wrong-but-valid pubkey in the
    /// url makes CB's signature validation reject every bid from the real
    /// relay; `skip_sigverify = true` is then the only way bids flow).
    /// Kurtosis service DNS (`helix-relay-N:4040`) makes the literal url
    /// resolvable in-enclave without knowing the relay's IP at generate time.
    pub literal_relay_url: Option<String>,
    /// When `Some`, append the `[signer]` + `[[modules]]` blocks. OPT-IN so the
    /// nine existing golden fixtures stay byte-identical: the signer sections
    /// are appended AFTER `[logs.file]`, which is valid TOML (interleaving is
    /// not, once `[[relays]]` has opened an array-of-tables).
    pub signer: Option<SignerParams>,
}

/// The `[signer]` + `[[modules]]` knobs. Deliberately minimal: everything CB
/// does not require is left at its default.
///
/// The key PATHS are intentionally absent - CB reads them from
/// `CB_SIGNER_LOADER_KEYS_DIR` / `CB_SIGNER_LOADER_SECRETS_DIR`, which override
/// the TOML (`signer/loader.rs`). That matters because the paths are
/// per-participant (`node-<idx>-keystores/...`) and the config template only
/// carries `.Network/.Port/.Relays/.Timestamp`, so they could not be templated
/// in anyway. The TOML keeps placeholder paths purely to satisfy the schema.
#[derive(Debug, Clone)]
pub struct SignerParams {
    /// Listen port. CB defaults to 20000.
    pub port: u16,
    /// Keystore format. **Must be `teku`** on a Kurtosis devnet: the
    /// ethereum-package's `secrets/` dir is `chmod 0600 -R` (mode 600,
    /// root-owned, NO execute bit - verified live), so the CB container's uid
    /// 10001 cannot traverse it and would load ZERO keys while looking healthy.
    /// `teku-secrets` is 755 and `teku-keys` is 777, which is also the pair the
    /// package's own web3signer launcher uses.
    pub keys_format: String,
    /// The single commit module. At least one `[[modules]]` entry is REQUIRED:
    /// with none, `load_module_signing_configs` bails loudly, and with an empty
    /// list the service exits 0 silently.
    pub module_id: String,
    /// 32-byte hex, non-zero, unique per module; mixed into the signing root.
    pub module_signing_id: String,
}

impl SignerParams {
    /// The devnet defaults: port 20000, teku keystores, one commit module.
    pub fn devnet() -> Self {
        Self {
            port: 20000,
            keys_format: "teku".to_string(),
            module_id: "TEST_MODULE".to_string(),
            // Arbitrary but fixed: a stable signing_id keeps BLS signatures
            // deterministic across runs, which is what the signature
            // differential assertion relies on.
            module_signing_id: "0x6a33a23ef26a4836979edff86c493a69b26ccf0b4a16491a815a13787657431b"
                .to_string(),
        }
    }
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
            literal_relay_url: None,
            signer: None,
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
    match &p.literal_relay_url {
        // Fault-injection: one literal [[relays]] entry, no template loop.
        Some(url) => {
            lines.push("[[relays]]".to_string());
            lines.push(r#"id = "mev_relay_0""#.to_string());
            lines.push(format!(r#"url = "{url}""#));
            for line in &p.per_relay_lines {
                lines.push(line.clone());
            }
        }
        None => {
            lines.push("{{ range $index, $relay := .Relays }}".to_string());
            lines.push("[[relays]]".to_string());
            lines.push(r#"id = "mev_relay_{{$index}}""#.to_string());
            lines.push(r#"url = "{{ $relay }}""#.to_string());

            for line in &p.per_relay_lines {
                lines.push(line.clone());
            }

            lines.push("{{- end }}".to_string());
        }
    }
    lines.push(String::new());
    lines.push("[logs.stdout]".to_string());
    lines.push(r#"level = "debug""#.to_string());
    lines.push(String::new());
    lines.push("[logs.file]".to_string());
    lines.push("enabled = false".to_string());

    if let Some(sg) = &p.signer {
        lines.push(String::new());
        lines.push("[signer]".to_string());
        lines.push(format!("port = {}", sg.port));
        lines.push(String::new());
        lines.push("[signer.local.loader]".to_string());
        // Placeholders only: CB_SIGNER_LOADER_{KEYS,SECRETS}_DIR override these
        // at runtime with the real per-participant artifact paths.
        lines.push(format!(r#"format = "{}""#, sg.keys_format));
        lines.push(r#"keys_path = "/keystores/teku-keys""#.to_string());
        lines.push(r#"secrets_path = "/keystores/teku-secrets""#.to_string());
        lines.push(String::new());
        lines.push("[[modules]]".to_string());
        lines.push(format!(r#"id = "{}""#, sg.module_id));
        lines.push(r#"type = "commit""#.to_string());
        lines.push(format!(r#"signing_id = "{}""#, sg.module_signing_id));
        // Required field, never read by the running signer (only by `cb init`).
        lines.push(r#"docker_image = "unused""#.to_string());
    }

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
            literal_relay_url: None,
            signer: None,
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
    fn signer_blocks_are_opt_in_and_change_nothing_when_absent() {
        // The whole point of making it Option: the nine pre-existing goldens
        // must stay byte-identical, so a signer scenario cannot churn them.
        let without = cb_toml(&CbParams::basic());
        assert!(!without.contains("[signer]"));
        assert!(!without.contains("[[modules]]"));
    }

    #[test]
    fn signer_blocks_render_after_the_logs_sections() {
        // TOML ordering is load-bearing: once [[relays]] has opened an
        // array-of-tables, interleaving new top-level tables is invalid.
        // Appending after [logs.file] is the only safe placement.
        let out = cb_toml(&CbParams {
            signer: Some(SignerParams::devnet()),
            ..CbParams::basic()
        });
        let relays_at = out.find("[[relays]]").expect("relays present");
        let logs_at = out.find("[logs.file]").expect("logs present");
        let signer_at = out.find("[signer]").expect("signer present");
        let modules_at = out.find("[[modules]]").expect("modules present");
        assert!(relays_at < logs_at, "relays before logs");
        assert!(logs_at < signer_at, "signer AFTER the logs sections");
        assert!(signer_at < modules_at, "[signer] before [[modules]]");
    }

    #[test]
    fn signer_uses_teku_keystores_not_the_unreadable_lighthouse_pair() {
        // Verified live: the package's secrets/ dir is mode 600 root-owned (no
        // execute bit), so CB's uid 10001 cannot traverse it and would start
        // healthy holding ZERO keys. teku-secrets is 755, teku-keys 777.
        let out = cb_toml(&CbParams {
            signer: Some(SignerParams::devnet()),
            ..CbParams::basic()
        });
        assert!(
            out.contains(r#"format = "teku""#),
            "must be the teku format"
        );
        assert!(out.contains("teku-keys") && out.contains("teku-secrets"));
        assert!(
            !out.contains(r#"keys_path = "/keystores/keys""#),
            "must NOT use the unreadable lighthouse keys/secrets pair"
        );
    }

    #[test]
    fn signer_emits_a_module_because_none_is_fatal() {
        // With NO [[modules]] the signer bails loudly; with an EMPTY list it
        // exits 0 silently. Either way a module entry is mandatory, and every
        // field of it is required (no serde defaults).
        let out = cb_toml(&CbParams {
            signer: Some(SignerParams::devnet()),
            ..CbParams::basic()
        });
        assert!(out.contains(r#"id = "TEST_MODULE""#));
        assert!(out.contains(r#"type = "commit""#));
        assert!(
            out.contains("signing_id = \"0x"),
            "signing_id must be present and hex"
        );
        assert!(
            out.contains(r#"docker_image = "unused""#),
            "required even though unread"
        );
        // signing_id must be non-zero: CB rejects the zero value.
        assert!(!out.contains(&format!("signing_id = \"0x{}\"", "0".repeat(64))));
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
