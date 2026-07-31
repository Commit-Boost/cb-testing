//! Scenario assembly — the `Scenario` enum + `Images` map that join the vetted
//! static fragments (participants, additional_services, network_params) with the
//! helix const and the CB block into a full Kurtosis args-file.
//!
//! Ports the Python `generate_*()` assemblers + `build_mev_params`
//! (`scripts/generate_kurtosis_configs.py`). The static fragments are kept as
//! verbatim `const` strings (they never drift; out of the port's typing scope).

use std::path::Path;

use eyre::{Result, WrapErr};

use super::cb::{cb_toml, cb_toml_mux, CbParams};
use super::helix::HELIX_RELAY_CONFIG;

// --- Vetted static fragments (verbatim from Python) -------------------------

const COMMON_PARTICIPANTS: &str = "participants:\n  - el_type: geth\n    cl_type: lighthouse";

const COMMON_ADDITIONAL_SERVICES: &str =
    "additional_services:\n  - dora\n  - spamoor\n  - prometheus";

const COMMON_NETWORK_PARAMS: &str = r#"network_params:
  network: kurtosis
  network_id: "3151908"
  deposit_contract_address: "0x00000000219ab540356cBB839Cbe05303d7705Fa"
  seconds_per_slot: 12
  slot_duration_ms: 12000
  num_validator_keys_per_node: 128
  preregistered_validator_keys_mnemonic:
    "giant issue aisle success illegal bike spike
    question tent bar rely arctic volcano long crawl hungry vocal artwork sniff fantasy
    very lucky have athlete"
  prefunded_accounts: '{"0xb9e79d19f651a941757b35830232E7EFC77E1c79": {"balance": "100000ETH"}}'
"#;

const MUX_NETWORK_PARAMS: &str = r#"network_params:
  network: kurtosis
  network_id: "3151908"
  deposit_contract_address: "0x00000000219ab540356cBB839Cbe05303d7705Fa"
  seconds_per_slot: 12
  slot_duration_ms: 12000
  num_validator_keys_per_node: 256
  preregistered_validator_keys_mnemonic:
    "giant issue aisle success illegal bike spike
    question tent bar rely arctic volcano long crawl hungry vocal artwork sniff fantasy
    very lucky have athlete"
  prefunded_accounts: '{"0xb9e79d19f651a941757b35830232E7EFC77E1c79": {"balance": "100000ETH"}}'
"#;

// --- Images (the ONE image map) ---------------------------------------------

/// The unified Docker-image map. Defaults are the baked, proven-good values —
/// note `mev_boost` = `commit-boost/commit-boost:kurtosis` (the Python's
/// `commit-boost/pbs:kurtosis` default was the bug this consolidation fixes).
/// `.env` overrides are applied at the CLI boundary (`generate::run`), never here.
#[derive(Debug, Clone)]
pub struct Images {
    pub helix_relay: String,
    pub mev_relay: String,
    pub mev_boost: String,
    pub builder_el: String,
    pub builder_cl: String,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            helix_relay: "ghcr.io/gattaca-com/helix-relay:main".to_string(),
            mev_relay: "ethpandaops/mev-boost-relay:main".to_string(),
            mev_boost: "commit-boost/commit-boost:kurtosis".to_string(),
            builder_el: "ethpandaops/reth-rbuilder:develop".to_string(),
            builder_cl: "sigp/lighthouse:latest".to_string(),
        }
    }
}

// --- Scenario ---------------------------------------------------------------

/// The six Commit-Boost test scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Basic,
    MultipleRelays,
    SkipSigverify,
    ExtraValidation,
    TimingGames,
    Mux,
}

impl Scenario {
    /// All six scenarios, in the Python emission order (the `scenarios` dict at
    /// `generate_kurtosis_configs.py:562`: timing-games precedes extra-validation).
    pub const ALL: [Scenario; 6] = [
        Scenario::Basic,
        Scenario::MultipleRelays,
        Scenario::SkipSigverify,
        Scenario::TimingGames,
        Scenario::ExtraValidation,
        Scenario::Mux,
    ];

    /// The scenario's canonical name (also the golden fixture / output basename).
    pub fn name(&self) -> &'static str {
        match self {
            Scenario::Basic => "cb-basic",
            Scenario::MultipleRelays => "cb-multiple-relays",
            Scenario::SkipSigverify => "cb-skip-sigverify",
            Scenario::ExtraValidation => "cb-extra-validation",
            Scenario::TimingGames => "cb-timing-games",
            Scenario::Mux => "cb-mux",
        }
    }

    /// Parse a scenario by name; `None` if unknown.
    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The leading comment block (verbatim from Python).
    fn comment(&self) -> &'static str {
        match self {
            Scenario::Basic => {
                "# cb-basic: Single relay (helix) with default Commit-Boost config.\n\
                 #\n\
                 # Tests the core MEV pipeline through Commit-Boost with a single Helix\n\
                 # relay as the only relay endpoint."
            }
            Scenario::MultipleRelays => {
                "# cb-multiple-relays: Two relays (helix + flashbots) behind a single\n\
                 # Commit-Boost sidecar.\n\
                 #\n\
                 # Tests that CB correctly routes get_header requests to both relays,\n\
                 # aggregating responses and selecting the best bid."
            }
            Scenario::SkipSigverify => {
                "# cb-skip-sigverify: Signature verification disabled for header responses.\n\
                 #\n\
                 # Tests the CB fast path where BLS verification is skipped. This trades\n\
                 # correctness for speed — useful to verify that the path exists and is\n\
                 # reachable under load."
            }
            Scenario::ExtraValidation => {
                "# cb-extra-validation: Enable extra validation of get_header responses\n\
                 # via a local execution layer client.\n\
                 #\n\
                 # Tests that CB will RPC-call the execution client to verify block\n\
                 # parameters before returning a header to the beacon node."
            }
            Scenario::TimingGames => {
                "# cb-timing-games: Aggressive timing game configuration.\n\
                 #\n\
                 # Tests CB's ability to orchestrate repeated get_header polls with\n\
                 # short timeouts in order to arrive at the best bid as late as possible\n\
                 # in the slot. Per-relay timing overrides are enabled for all relays."
            }
            Scenario::Mux => {
                "# cb-mux: Multiplexed relay routing per validator node.\n\
                 #\n\
                 # Routes all 128 validators from node-0 exclusively to the Helix relay and\n\
                 # all 128 validators from node-1 exclusively to the Flashbots relay.\n\
                 # This tests CB's ability to partition the validator set and apply\n\
                 # per-mux timeout and relay configurations."
            }
        }
    }

    /// The relay list. Single-relay scenarios emit `mev_relay: <name>` (scalar)
    /// and OMIT `mev_relay_image`; multi-relay scenarios emit a list and INCLUDE
    /// `mev_relay_image` — matching the Python scenario dicts.
    fn relays(&self) -> &'static [&'static str] {
        match self {
            Scenario::Basic | Scenario::SkipSigverify | Scenario::ExtraValidation => &["helix"],
            Scenario::MultipleRelays | Scenario::TimingGames | Scenario::Mux => {
                &["helix", "flashbots"]
            }
        }
    }

    /// The commit-boost TOML block for this scenario. `keys_dir` is only read for
    /// the mux scenario (the 256 per-node pubkey lists).
    fn cb_block(&self, keys_dir: &Path) -> Result<String> {
        Ok(match self {
            Scenario::Basic | Scenario::MultipleRelays => cb_toml(&CbParams::basic()),
            Scenario::SkipSigverify => cb_toml(&CbParams {
                extra_pbs_lines: vec!["skip_sigverify = true".to_string()],
                ..CbParams::basic()
            }),
            Scenario::ExtraValidation => cb_toml(&CbParams {
                extra_pbs_lines: vec![
                    "extra_validation_enabled = true".to_string(),
                    r#"rpc_url = "http://el-1-geth-lighthouse:8545""#.to_string(),
                ],
                ..CbParams::basic()
            }),
            Scenario::TimingGames => cb_toml(&CbParams {
                timeout_get_header_ms: 400,
                timeout_get_payload_ms: 2000,
                extra_pbs_lines: Vec::new(),
                per_relay_lines: vec![
                    "enable_timing_games = true".to_string(),
                    "target_first_request_ms = 100".to_string(),
                    "frequency_get_header_ms = 200".to_string(),
                ],
            }),
            Scenario::Mux => {
                let node0 = load_pubkeys(keys_dir, 0)?;
                let node1 = load_pubkeys(keys_dir, 1)?;
                cb_toml_mux(&node0, &node1)
            }
        })
    }

    fn network_params(&self) -> &'static str {
        match self {
            Scenario::Mux => MUX_NETWORK_PARAMS,
            _ => COMMON_NETWORK_PARAMS,
        }
    }

    /// Assemble the full Kurtosis args-file for this scenario. Reads
    /// `keys/node-{0,1}-pubkeys.json` under `keys_dir` for the mux scenario only.
    pub fn args_file_in(&self, images: &Images, keys_dir: &Path) -> Result<String> {
        let cb_block = self.cb_block(keys_dir)?;
        let mev_params = build_mev_params(self.relays(), images, &cb_block);
        Ok([
            self.comment(),
            COMMON_PARTICIPANTS,
            COMMON_ADDITIONAL_SERVICES,
            "mev_type: custom",
            &mev_params,
            self.network_params(),
        ]
        .join("\n\n")
            + "\n")
    }
}

// --- mev_params assembly (ports build_mev_params) ---------------------------

fn build_mev_params(relays: &[&str], images: &Images, cb_block: &str) -> String {
    let mut lines: Vec<String> = vec!["mev_params:".to_string()];

    if relays.len() > 1 {
        lines.push("  mev_relay:".to_string());
        for r in relays {
            lines.push(format!("    - {r}"));
        }
    } else {
        lines.push(format!("  mev_relay: {}", relays[0]));
    }

    lines.push("  mev_sidecar: commit-boost".to_string());
    lines.push("  mev_builder: flashbots".to_string());
    lines.push(String::new());

    // Image map. Single-relay scenarios omit mev_relay_image (correlates with the
    // scalar relay form above).
    lines.push(format!("  helix_relay_image: {}", images.helix_relay));
    if relays.len() > 1 {
        lines.push(format!("  mev_relay_image: {}", images.mev_relay));
    }
    lines.push(format!("  mev_boost_image: {}", images.mev_boost));
    lines.push(format!("  mev_builder_image: {}", images.builder_el));
    lines.push(format!("  mev_builder_cl_image: {}", images.builder_cl));

    lines.push(String::new());
    lines.push("  mev_builder_subsidy: 1".to_string());
    lines.push(String::new());

    // helix block scalar: indent non-empty lines 4 spaces, blanks stay empty.
    lines.push("  helix_relay_config: |".to_string());
    push_block_scalar(&mut lines, HELIX_RELAY_CONFIG);
    lines.push(String::new());

    // commit-boost block scalar.
    lines.push("  commit_boost_config: |".to_string());
    push_block_scalar(&mut lines, cb_block);

    lines.join("\n")
}

/// Append `body` as a YAML `|` block scalar body: non-empty lines get a 4-space
/// indent, blank lines stay truly empty (matches Python `build_mev_params`).
fn push_block_scalar(lines: &mut Vec<String>, body: &str) {
    for line in body.lines() {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.push(format!("    {line}"));
        }
    }
}

/// Load `keys_dir/node-{node}-pubkeys.json` as a list of pubkey strings. Fallible
/// so a missing/malformed keys file surfaces as a clean `run` error (and lets the
/// caller validate BEFORE writing anything), matching the Python's pre-write
/// `load_pubkeys` + `sys.exit(1)` rather than panicking mid-generation.
fn load_pubkeys(keys_dir: &Path, node: u8) -> Result<Vec<String>> {
    let path = keys_dir.join(format!("node-{node}-pubkeys.json"));
    let raw = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("reading pubkey file {}", path.display()))?;
    serde_json::from_str(&raw)
        .wrap_err_with(|| format!("parsing pubkey JSON {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genmodel::assert_matches_golden;

    /// Headline test: every scenario assembled with the default images must be
    /// byte-identical to its golden fixture. The mux scenario exercises the full
    /// 256-key path (real `keys/*.json`).
    #[test]
    fn every_scenario_matches_its_golden() {
        let images = Images::default();
        for s in Scenario::ALL {
            let produced = s.args_file_in(&images, Path::new("keys")).unwrap();
            assert_matches_golden(s.name(), &produced);
        }
    }

    #[test]
    fn from_name_round_trips() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::from_name(s.name()), Some(s));
        }
        assert_eq!(Scenario::from_name("nope"), None);
    }

    #[test]
    fn mux_with_missing_keys_is_a_clean_error_not_a_panic() {
        // The mux scenario reads keys/*.json; a missing dir must surface as an
        // Err (which `run` turns into a clean exit), never a panic mid-generation.
        let err = Scenario::Mux
            .args_file_in(&Images::default(), Path::new("/no/such/keys"))
            .unwrap_err();
        assert!(err.to_string().contains("pubkey file"), "got: {err}");
    }

    #[test]
    fn non_mux_scenarios_need_no_keys() {
        // Everything except mux must assemble regardless of the keys dir.
        for s in Scenario::ALL {
            if s == Scenario::Mux {
                continue;
            }
            assert!(s.args_file_in(&Images::default(), Path::new("/no/such/keys")).is_ok());
        }
    }
}
