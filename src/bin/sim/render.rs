//! Shared runtime-var substitution + config-block extraction for the `sim`
//! preflight (Task 1).
//!
//! The kurtosis args-file embeds two configs as YAML `|` block scalars under
//! `mev_params`: `helix_relay_config` (YAML) and `commit_boost_config` (TOML).
//! Both carry unrendered Go-template vars (`{{ .VAR }}`), and the CB block has a
//! `{{ range $i, $r := .Relays }} … {{- end }}` loop. Un-substituted, neither
//! block is parseable. This module renders dummy values in so later tasks can
//! validate the shapes.
//!
//! Pure: string in, string out. No fs/process (the test harness reads the real
//! fixture; the module itself never touches I/O).

use std::collections::BTreeMap;

/// The two embedded config bodies pulled out of the args-file.
pub struct ConfigBlocks {
    /// The `helix_relay_config` body (YAML, with template vars unrendered).
    pub helix: String,
    /// The `commit_boost_config` body (TOML, with template vars + a `.Relays`
    /// range loop unrendered).
    pub commit_boost: String,
}

/// Pull the two `|` block scalars out of the kurtosis args-file.
///
/// The `|` bodies are opaque scalars to YAML, so `serde_yaml` hands them back as
/// plain strings — exactly the un-rendered template text we want to substitute
/// into.
pub fn extract_config_blocks(args_file_contents: &str) -> eyre::Result<ConfigBlocks> {
    let root: serde_yaml::Value = serde_yaml::from_str(args_file_contents)?;
    let mev = root
        .get("mev_params")
        .ok_or_else(|| eyre::eyre!("args-file has no `mev_params` mapping"))?;

    let helix = mev
        .get("helix_relay_config")
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| eyre::eyre!("`mev_params.helix_relay_config` missing or not a string"))?
        .to_string();

    let commit_boost = mev
        .get("commit_boost_config")
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| eyre::eyre!("`mev_params.commit_boost_config` missing or not a string"))?
        .to_string();

    Ok(ConfigBlocks {
        helix,
        commit_boost,
    })
}

/// Render a config block: strip any `{{ range … }} … {{- end }}` loop, then
/// replace every `{{ .VAR }}` with its dummy.
///
/// Whitespace inside the markers is tolerated (`{{.VAR}}`, `{{ .VAR }}`). A
/// `{{ .VAR }}` with no dummy is left verbatim on purpose: the caller's
/// `!contains("{{")` check then flags it as a missing dummy rather than silently
/// emitting a broken value.
pub fn substitute_runtime_vars(block: &str, dummies: &BTreeMap<&str, String>) -> String {
    let stripped = strip_range_blocks(block);
    replace_simple_vars(&stripped, dummies)
}

/// Remove whole `{{ range … }} … {{- end }}` (or `{{ end }}`) line ranges.
///
/// Go's `{{-` trim marker only affects surrounding whitespace, which line-range
/// removal already discards, so we treat `{{- end }}` and `{{ end }}` alike. The
/// `.Relays` are `#[serde(default)]` downstream, so dropping the loop yields a
/// valid empty-relays config.
fn strip_range_blocks(block: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;

    for line in block.lines() {
        if skipping {
            // Inside a range block: drop everything up to and including `end`.
            if is_range_end(line) {
                skipping = false;
            }
            continue;
        }

        if is_range_start(line) {
            // A degenerate one-line `{{ range … }}{{ end }}` closes immediately.
            skipping = !is_range_end(line);
            continue;
        }

        kept.push(line);
    }

    let mut out = kept.join("\n");
    if block.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn is_range_start(line: &str) -> bool {
    line.contains("{{") && line.contains("range")
}

fn is_range_end(line: &str) -> bool {
    line.contains("end") && line.contains("}}")
}

/// Replace each `{{ .VAR }}` marker with its dummy, tolerating internal
/// whitespace. Markers whose key has no dummy are emitted verbatim.
fn replace_simple_vars(block: &str, dummies: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(block.len());
    let mut rest = block;

    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];

        let Some(close_rel) = after_open.find("}}") else {
            // Unbalanced marker: keep the tail untouched and stop.
            out.push_str(&rest[open..]);
            return out;
        };

        let inner = &after_open[..close_rel];
        let key = inner.trim().trim_start_matches('.');

        match dummies.get(key) {
            Some(val) => out.push_str(val),
            // Unknown var: keep the raw marker so the caller's check catches it.
            None => out.push_str(&rest[open..open + 2 + close_rel + 2]),
        }

        rest = &after_open[close_rel + 2..];
    }

    out.push_str(rest);
    out
}

/// Type-correct dummy values for every runtime var the real args-file uses.
///
/// Numeric-context vars (`Port`, `POSTGRES_PORT`, `Timestamp`) are bare digit
/// strings so they render unquoted (`port: 5432`, `genesis_time_secs = 170…`);
/// URI vars are valid URLs; the rest are plain names that fit their quoted slot.
pub fn default_dummies() -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    // Helix (YAML) vars.
    m.insert("POSTGRES_HOST_NAME", "postgres".to_string());
    m.insert("POSTGRES_PORT", "5432".to_string());
    m.insert("POSTGRES_DB", "helix".to_string());
    m.insert("POSTGRES_USER", "helix".to_string());
    m.insert("POSTGRES_PASS", "helixpass".to_string());
    m.insert("BEACON_URI", "http://127.0.0.1:5052".to_string());
    m.insert("BLOCKSIM_URI", "http://127.0.0.1:8545".to_string());
    // Commit-Boost (TOML) vars.
    m.insert("Timestamp", "1700000000".to_string());
    m.insert("Network", "mainnet".to_string());
    m.insert("Port", "18550".to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real, checked-in args-file — the only trustworthy fixture for the
    /// exact template shapes we must handle.
    const ARGS_FILE: &str = include_str!("../../../configs/generated/cb-basic.yml");

    #[test]
    fn test_extract_both_blocks() {
        let blocks = extract_config_blocks(ARGS_FILE).expect("extract");

        // helix body starts with the helix config's first line.
        assert!(
            blocks.helix.trim_start().starts_with("instance_id:"),
            "helix block should start with instance_id, got:\n{}",
            &blocks.helix[..blocks.helix.len().min(80)]
        );
        // commit_boost body carries the [pbs] table.
        assert!(
            blocks.commit_boost.contains("[pbs]"),
            "commit_boost block should contain [pbs]"
        );
        // Neither block bleeds into the other.
        assert!(
            !blocks.helix.contains("[pbs]"),
            "helix block must not contain the CB key [pbs]"
        );
        assert!(
            !blocks.commit_boost.contains("instance_id"),
            "commit_boost block must not contain the helix key instance_id"
        );
    }

    #[test]
    fn test_substitute_covers_all_vars() {
        let blocks = extract_config_blocks(ARGS_FILE).expect("extract");
        let dummies = default_dummies();

        for (name, block) in [
            ("helix", &blocks.helix),
            ("commit_boost", &blocks.commit_boost),
        ] {
            let rendered = substitute_runtime_vars(block, &dummies);
            assert!(
                !rendered.contains("{{"),
                "{name} block still has an opening template marker after substitution:\n{rendered}"
            );
            assert!(
                !rendered.contains("}}"),
                "{name} block still has a closing template marker after substitution:\n{rendered}"
            );
        }
    }

    #[test]
    fn test_range_block_stripped() {
        let blocks = extract_config_blocks(ARGS_FILE).expect("extract");
        let rendered = substitute_runtime_vars(&blocks.commit_boost, &default_dummies());

        assert!(
            !rendered.contains("[[relays]]"),
            "the .Relays range body should be stripped, got:\n{rendered}"
        );
        assert!(
            !rendered.contains("range"),
            "no `range` residue should survive"
        );
        assert!(!rendered.contains("end"), "no `end` residue should survive");
    }

    #[test]
    fn test_substituted_helix_is_valid_yaml() {
        let blocks = extract_config_blocks(ARGS_FILE).expect("extract");
        let rendered = substitute_runtime_vars(&blocks.helix, &default_dummies());

        serde_yaml::from_str::<serde_yaml::Value>(&rendered).unwrap_or_else(|e| {
            panic!("substituted helix is not valid YAML: {e}\n---\n{rendered}")
        });
    }

    #[test]
    fn test_substituted_cb_is_valid_toml() {
        let blocks = extract_config_blocks(ARGS_FILE).expect("extract");
        let rendered = substitute_runtime_vars(&blocks.commit_boost, &default_dummies());

        toml::from_str::<toml::Value>(&rendered)
            .unwrap_or_else(|e| panic!("substituted CB is not valid TOML: {e}\n---\n{rendered}"));
    }
}
