//! Scenario assembly — the `Scenario` enum + `Images` map that join the vetted
//! static fragments (participants, additional_services, network_params) with the
//! helix const and the CB block into a full Kurtosis args-file.
//!
//! Ports the Python `generate_*()` assemblers + `build_mev_params`
//! (`scripts/generate_kurtosis_configs.py`). The static fragments are kept as
//! verbatim `const` strings (they never drift; out of the port's typing scope).

use std::path::Path;

use eyre::{Result, WrapErr};

use super::cb::{CbParams, cb_toml, cb_toml_mux};
use super::helix::HELIX_RELAY_CONFIG;

// --- Vetted static fragments (verbatim from Python) -------------------------

/// An execution/consensus client pair. Law 7 ("coverage is a matrix, not a
/// point"): everything used to hardcode geth+lighthouse, so a CB regression
/// specific to another pair was invisible. The pair is threaded through BOTH
/// the participants block and every service name derived from it — notably
/// extra-validation's `rpc_url`, which the ethereum-package names
/// `el-{index}-{el}-{cl}` (`src/el/el_launcher.star:177`). That coupling is
/// exactly why a hardcoded rpc_url silently no-ops on an alternate pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElCl {
    pub el: &'static str,
    pub cl: &'static str,
}

impl ElCl {
    /// The baked default pair every scenario used before Law 7.
    pub const DEFAULT: ElCl = ElCl {
        el: "geth",
        cl: "lighthouse",
    };
    /// The alternate pair (the P3 slice: prove the parametrization is real).
    pub const ALT: ElCl = ElCl {
        el: "nethermind",
        cl: "prysm",
    };

    /// The `participants:` fragment for this pair.
    fn participants(&self) -> String {
        format!(
            "participants:\n  - el_type: {}\n    cl_type: {}",
            self.el, self.cl
        )
    }

    /// The first participant's EL RPC endpoint, as the ethereum-package names
    /// it (`el-{index}-{el}-{cl}`, 1-indexed).
    fn el_rpc_url(&self) -> String {
        format!("http://el-1-{}-{}:8545", self.el, self.cl)
    }
}

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
    BasicAltClients,
    MinBid,
    SkipSigverify,
    SigverifyDiff,
    SigverifyDiffControl,
    ExtraValidation,
    TimingGames,
    Mux,
}

impl Scenario {
    /// All six scenarios, in the Python emission order (the `scenarios` dict at
    /// `generate_kurtosis_configs.py:562`: timing-games precedes extra-validation).
    pub const ALL: [Scenario; 10] = [
        Scenario::Basic,
        Scenario::BasicAltClients,
        Scenario::MultipleRelays,
        Scenario::MinBid,
        Scenario::SkipSigverify,
        Scenario::SigverifyDiff,
        Scenario::SigverifyDiffControl,
        Scenario::TimingGames,
        Scenario::ExtraValidation,
        Scenario::Mux,
    ];

    /// The scenario's canonical name (also the golden fixture / output basename).
    pub fn name(&self) -> &'static str {
        match self {
            Scenario::Basic => "cb-basic",
            Scenario::MultipleRelays => "cb-multiple-relays",
            Scenario::BasicAltClients => "cb-basic-nethermind-prysm",
            Scenario::MinBid => "cb-min-bid",
            Scenario::SkipSigverify => "cb-skip-sigverify",
            Scenario::SigverifyDiff => "cb-sigverify-diff",
            Scenario::SigverifyDiffControl => "cb-sigverify-diff-control",
            Scenario::ExtraValidation => "cb-extra-validation",
            Scenario::TimingGames => "cb-timing-games",
            Scenario::Mux => "cb-mux",
        }
    }

    /// Parse a scenario by name; `None` if unknown.
    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The EL/CL client pair this scenario runs on (Law 7). Everything else
    /// stays on the baked default pair; the alt-clients scenario is the P3
    /// slice proving the parametrization is real end to end.
    pub fn el_cl(&self) -> ElCl {
        match self {
            Scenario::BasicAltClients => ElCl::ALT,
            _ => ElCl::DEFAULT,
        }
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
                "# cb-multiple-relays: Two Helix relay instances behind a single\n\
                 # Commit-Boost sidecar.\n\
                 #\n\
                 # Tests that CB correctly routes get_header requests to both relays,\n\
                 # aggregating responses and selecting the best bid. The per-relay\n\
                 # subsidy list [1, 2] makes the builder submit DIVERGENT bid values\n\
                 # (rbuilder [[subsidy_overrides]]), so the best-bid selection is a\n\
                 # real discrimination, not a tie between identical bids."
            }
            Scenario::BasicAltClients => {
                "# cb-basic-nethermind-prysm: cb-basic on an ALTERNATE EL/CL pair.\n\
                 #\n\
                 # Law 7 (coverage is a matrix, not a point): every other scenario runs\n\
                 # geth+lighthouse, so a CB regression specific to another client pair is\n\
                 # invisible. Same MEV pipeline assertions as cb-basic, different clients."
            }
            Scenario::MinBid => {
                "# cb-min-bid: the min_bid_eth floor actually drops bids.\n\
                 #\n\
                 # min_bid_eth = 0.5 with the builder subsidy OFF: real devnet bids are\n\
                 # ~0.04 ETH of spamoor MEV, so EVERY bid must be rejected with\n\
                 # \"bid below minimum\" and zero auctions won. The subsidy must be 0 or\n\
                 # bids land near 1.04 ETH and no LEGAL floor could reject them - CB\n\
                 # validates min_bid_wei < 1 ETH.\n\
                 #\n\
                 # Doubles as a canary for CB's silent-flatten trap: [pbs] has no\n\
                 # deny_unknown_fields, so a renamed/misspelled key is IGNORED rather\n\
                 # than rejected. If bids still win here, the key was silently dropped."
            }
            Scenario::SkipSigverify => {
                "# cb-skip-sigverify: Signature verification disabled for header responses.\n\
                 #\n\
                 # Tests the CB fast path where BLS verification is skipped. This trades\n\
                 # correctness for speed — useful to verify that the path exists and is\n\
                 # reachable under load."
            }
            Scenario::SigverifyDiff => {
                "# cb-sigverify-diff: the skip_sigverify DIFFERENTIAL (treatment arm).\n\
                 #\n\
                 # CB's [[relays]] entry is a LITERAL url whose pubkey is a valid BLS\n\
                 # key that is NOT the helix relay's signing key, so CB's signature\n\
                 # validation would reject every bid. With skip_sigverify = true the\n\
                 # validation is skipped and bids flow anyway - an auction winner in\n\
                 # this scenario is positive proof the skip codepath fired. Compare\n\
                 # with cb-sigverify-diff-control (same poison, skip OFF, zero bids)."
            }
            Scenario::SigverifyDiffControl => {
                "# cb-sigverify-diff-control: the skip_sigverify differential (control\n\
                 # arm). Same wrong-pubkey literal relay url as cb-sigverify-diff but\n\
                 # withOUT skip_sigverify - CB rejects every bid (PubkeyMismatch), so\n\
                 # the run is EXPECTED to fail payload delivery. `sim diff` against the\n\
                 # treatment run shows the flip that proves the feature discriminates."
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
                 # Routes all 128 validators from node-0 exclusively to the first Helix\n\
                 # relay instance and all 128 validators from node-1 exclusively to the\n\
                 # second Helix relay instance. This tests CB's ability to partition the\n\
                 # validator set and apply per-mux timeout and relay configurations."
            }
        }
    }

    /// The relay list. Single-relay scenarios emit `mev_relay: <name>` (scalar)
    /// and OMIT `mev_relay_image`; multi-relay scenarios emit a list and INCLUDE
    /// `mev_relay_image` — matching the Python scenario dicts.
    fn relays(&self) -> &'static [&'static str] {
        match self {
            Scenario::Basic
            | Scenario::BasicAltClients
            | Scenario::MinBid
            | Scenario::SkipSigverify
            | Scenario::SigverifyDiff
            | Scenario::SigverifyDiffControl
            | Scenario::ExtraValidation => &["helix"],
            Scenario::MultipleRelays | Scenario::TimingGames | Scenario::Mux => &["helix", "helix"],
        }
    }

    /// The commit-boost TOML block for this scenario. `keys_dir` is only read for
    /// the mux scenario (the 256 per-node pubkey lists).
    fn cb_block(&self, keys_dir: &Path) -> Result<String> {
        Ok(match self {
            Scenario::Basic | Scenario::BasicAltClients | Scenario::MultipleRelays => {
                cb_toml(&CbParams::basic())
            }
            Scenario::MinBid => cb_toml(&CbParams {
                extra_pbs_lines: vec!["min_bid_eth = 0.5".to_string()],
                ..CbParams::basic()
            }),
            Scenario::SkipSigverify => cb_toml(&CbParams {
                extra_pbs_lines: vec!["skip_sigverify = true".to_string()],
                ..CbParams::basic()
            }),
            Scenario::SigverifyDiff => cb_toml(&CbParams {
                extra_pbs_lines: vec!["skip_sigverify = true".to_string()],
                literal_relay_url: Some(poisoned_relay_url()),
                ..CbParams::basic()
            }),
            Scenario::SigverifyDiffControl => cb_toml(&CbParams {
                literal_relay_url: Some(poisoned_relay_url()),
                ..CbParams::basic()
            }),
            Scenario::ExtraValidation => cb_toml(&CbParams {
                extra_pbs_lines: vec![
                    "extra_validation_enabled = true".to_string(),
                    format!(r#"rpc_url = "{}""#, self.el_cl().el_rpc_url()),
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
                literal_relay_url: None,
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

    /// The builder subsidy YAML value. cb-multiple-relays uses the per-relay
    /// LIST form ([1, 2]: relay 0 gets subsidy 1, relay 1 gets 2) so the two
    /// helix instances offer DIVERGENT bid values on the same slot and CB's
    /// best-bid aggregation is actually discriminated — with the scalar form
    /// one shared builder submits the identical bid to both relays and the
    /// comparison is degenerate. Other scenarios keep the historical scalar.
    fn builder_subsidy(&self) -> &'static str {
        match self {
            Scenario::MultipleRelays => "[1, 2]",
            // MUST be 0: with a 1 ETH subsidy every bid lands near 1.04 ETH and
            // CB caps min_bid_wei below 1 ETH, so no legal floor could reject
            // one and the scenario would silently prove nothing.
            Scenario::MinBid => "0",
            _ => "1",
        }
    }

    /// Assemble the full Kurtosis args-file for this scenario. Reads
    /// `keys/node-{0,1}-pubkeys.json` under `keys_dir` for the mux scenario only.
    pub fn args_file_in(&self, images: &Images, keys_dir: &Path) -> Result<String> {
        let cb_block = self.cb_block(keys_dir)?;
        let mev_params = build_mev_params(self.relays(), images, &cb_block, self.builder_subsidy());
        Ok([
            self.comment(),
            &self.el_cl().participants(),
            COMMON_ADDITIONAL_SERVICES,
            "mev_type: custom",
            &mev_params,
            self.network_params(),
        ]
        .join("\n\n")
            + "\n")
    }
}

// --- sigverify differential fault injection ---------------------------------

/// A VALID BLS pubkey (validator key from the standard preregistered mnemonic)
/// that is NOT the helix relay's signing key (`DEFAULT_MEV_PUBKEY` in the
/// ethereum-package, 0xa55c1285...). Putting it in CB's [[relays]] url makes
/// validate_signature reject every bid from the real relay (PubkeyMismatch) -
/// unless skip_sigverify is on. Must be a real curve point or CB fails at
/// config parse; a mnemonic validator key is guaranteed valid.
pub const WRONG_RELAY_PUBKEY: &str = "0xaaf6c1251e73fb600624937760fef218aace5b253bf068ed45398aeb29d821e4d2899343ddcbbe37cb3f6cf500dff26c";

/// The literal poisoned relay url. Service DNS: with the 1-participant common
/// scenario + the auto-appended builder participant, main.star launches the
/// single helix instance as `helix-relay-2` (index = participant_count 2 +
/// relay_index 0), listening on the fixed in-enclave port 4040 - confirmed by
/// the live 2-helix runs (helix-relay-2/-3).
fn poisoned_relay_url() -> String {
    format!("http://{WRONG_RELAY_PUBKEY}@helix-relay-2:4040")
}

// --- mev_params assembly (ports build_mev_params) ---------------------------

fn build_mev_params(relays: &[&str], images: &Images, cb_block: &str, subsidy: &str) -> String {
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
    lines.push(format!("  mev_builder_subsidy: {subsidy}"));
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
    serde_json::from_str(&raw).wrap_err_with(|| format!("parsing pubkey JSON {}", path.display()))
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
    fn alt_client_pair_flows_into_participants() {
        // Law 7: the pair is real config, not a label.
        let out = Scenario::BasicAltClients
            .args_file_in(&Images::default(), Path::new("keys"))
            .unwrap();
        assert!(
            out.contains("el_type: nethermind"),
            "alt EL in participants"
        );
        assert!(out.contains("cl_type: prysm"), "alt CL in participants");
        // Every other scenario stays on the baked default pair.
        let basic = Scenario::Basic
            .args_file_in(&Images::default(), Path::new("keys"))
            .unwrap();
        assert!(basic.contains("el_type: geth") && basic.contains("cl_type: lighthouse"));
    }

    #[test]
    fn extra_validation_rpc_url_derives_from_the_pair() {
        // The coupling Law 7 exists to catch: the ethereum-package names the EL
        // service el-{index}-{el}-{cl}, so a HARDCODED rpc_url silently points
        // at a nonexistent service on any other pair (extra validation then
        // no-ops, and feature.extra_validation would WARN). Derive it instead.
        assert_eq!(
            ElCl::DEFAULT.el_rpc_url(),
            "http://el-1-geth-lighthouse:8545"
        );
        assert_eq!(ElCl::ALT.el_rpc_url(), "http://el-1-nethermind-prysm:8545");
        // The default-pair scenario's rendered config still carries the
        // original url byte-for-byte (no silent drift from the refactor).
        let out = Scenario::ExtraValidation
            .args_file_in(&Images::default(), Path::new("keys"))
            .unwrap();
        assert!(out.contains(r#"rpc_url = "http://el-1-geth-lighthouse:8545""#));
    }

    #[test]
    fn from_name_round_trips() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::from_name(s.name()), Some(s));
        }
        assert_eq!(Scenario::from_name("nope"), None);
    }

    #[test]
    fn tracked_cb_basic_config_stays_in_sync_with_sim_generate() {
        // configs/generated/cb-basic.yml is TRACKED (render.rs's fixture + what
        // `sim preflight` validates). Guard it against silently drifting from the
        // generator — the staleness class that rotted the old example config.
        let tracked = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/configs/generated/cb-basic.yml"
        ));
        let produced = Scenario::Basic
            .args_file_in(&Images::default(), Path::new("keys"))
            .unwrap();
        assert_eq!(
            produced, tracked,
            "configs/generated/cb-basic.yml is stale — run `just generate-configs`"
        );
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
            assert!(
                s.args_file_in(&Images::default(), Path::new("/no/such/keys"))
                    .is_ok()
            );
        }
    }
}
