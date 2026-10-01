//! Which ethereum-package an args-file is written for.
//!
//! [`Target::Fork`] is the Commit-Boost fork of ethereum-package, the default and
//! what the goldens pin: `mev_type: custom` plus the fork's `(relay, sidecar,
//! builder)` resolver keys. [`Target::Defork`] is upstream ethpandaops
//! ethereum-package carrying the small patch set in `docs/defork-plan.md`, where
//! `mev_type: commit-boost` picks the commit-boost sidecar and the flashbots
//! builder, and `mev_params.mev_relays` replaces the relay set.
//!
//! How each fork-only key maps onto the de-forked package:
//!
//! | fork | de-forked |
//! |---|---|
//! | `mev_type: custom` | `mev_type: commit-boost` |
//! | `mev_relay: helix` (scalar or list) | `mev_relays`, always a list |
//! | `mev_sidecar: commit-boost`, `mev_builder: flashbots` | implied by `mev_type: commit-boost` |
//! | `mev_relay_image` | dropped: it is the flashbots relay's image, and no flashbots relay runs |
//! | CB `chain = { .., path = "{{ .Network }}" }` | an inline chain, see [`with_inline_chain`] |
//! | `commit_boost_extra_files` | unchanged |
//! | `commit_boost_signer` | none, see `ScenarioSpec::unsupported_keys` |

use std::path::Path;

use eyre::{Result, eyre};

use super::cb::CHAIN_FROM_SPEC;

/// The genesis fork version every Kurtosis devnet uses: `GENESIS_FORK_VERSION`
/// in the package's `src/package_io/constants.star`. It is not an args key, so
/// no args-file can change it.
const GENESIS_FORK_VERSION: &str = "0x10000038";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Target {
    #[default]
    Fork,
    Defork,
}

impl Target {
    /// The `mev_type` value that selects the helix relay + commit-boost sidecar
    /// + flashbots builder combination on this target's package.
    pub fn mev_type(self) -> &'static str {
        match self {
            Target::Fork => "custom",
            Target::Defork => "commit-boost",
        }
    }

    /// Where `sim generate` writes this target's configs by default. Separate
    /// dirs, so the two targets can be generated and run side by side.
    pub fn default_out_dir(self) -> &'static Path {
        match self {
            Target::Fork => Path::new("configs/generated"),
            Target::Defork => Path::new("configs/generated-defork"),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Target::Fork => "fork",
            Target::Defork => "de-forked",
        })
    }
}

/// Replace the CB config's leading chain line with an inline chain built from
/// `network_params` (the args-file's `network_params:` fragment).
///
/// The fork's line points CB at the chain spec the package's genesis generator
/// writes. Upstream's generator (6.2.x) writes `SLOT_DURATION_MS` and no longer
/// `SECONDS_PER_SLOT`, which CB's spec loader requires, so on the de-forked
/// package CB must be handed the chain inline.
pub fn with_inline_chain(cb_block: &str, network_params: &str) -> Result<String> {
    let rest = cb_block.strip_prefix(CHAIN_FROM_SPEC).ok_or_else(|| {
        eyre!("CB config does not open with the chain-spec line it is meant to replace")
    })?;
    Ok(format!("{}{rest}", inline_chain(network_params)?))
}

/// The inline `chain = {..}` line, with the slot time and chain id read from the
/// same `network_params` the args-file sets. `fulu_fork_slot` is 0 because the
/// package defaults `fulu_fork_epoch` to 0; a fragment that schedules fulu or
/// picks a preset is rejected rather than guessed at, since the slot would then
/// depend on the preset's epoch length.
fn inline_chain(network_params: &str) -> Result<String> {
    let root: serde_yaml::Value = serde_yaml::from_str(network_params)?;
    let params = root
        .get("network_params")
        .ok_or_else(|| eyre!("fragment has no `network_params` mapping"))?;
    for key in ["fulu_fork_epoch", "preset"] {
        eyre::ensure!(
            params.get(key).is_none(),
            "inline CB chain assumes the package default for `{key}`; the fragment sets it"
        );
    }
    let slot_secs = params
        .get("seconds_per_slot")
        .and_then(serde_yaml::Value::as_u64)
        .ok_or_else(|| eyre!("`network_params.seconds_per_slot` missing or not an integer"))?;
    let chain_id = params
        .get("network_id")
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| eyre!("`network_params.network_id` missing or not a string"))?;
    Ok(format!(
        "chain = {{ genesis_time_secs = {{{{ .Timestamp }}}}, slot_time_secs = {slot_secs}, \
         genesis_fork_version = \"{GENESIS_FORK_VERSION}\", fulu_fork_slot = 0, \
         chain_id = \"{chain_id}\" }}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genmodel::scenario::{COMMON_NETWORK_PARAMS, MUX_NETWORK_PARAMS};

    /// The line the live de-fork probe passed with (12s slots, devnet 3151908).
    const PROBE_CHAIN: &str = r#"chain = { genesis_time_secs = {{ .Timestamp }}, slot_time_secs = 12, genesis_fork_version = "0x10000038", fulu_fork_slot = 0, chain_id = "3151908" }"#;

    #[test]
    fn inline_chain_matches_the_live_probe_for_both_fragments() {
        assert_eq!(inline_chain(COMMON_NETWORK_PARAMS).unwrap(), PROBE_CHAIN);
        assert_eq!(inline_chain(MUX_NETWORK_PARAMS).unwrap(), PROBE_CHAIN);
    }

    #[test]
    fn inline_chain_reads_its_values_from_the_fragment() {
        let fragment = COMMON_NETWORK_PARAMS
            .replace("seconds_per_slot: 12", "seconds_per_slot: 6")
            .replace(r#"network_id: "3151908""#, r#"network_id: "42""#);
        let line = inline_chain(&fragment).unwrap();
        assert!(line.contains("slot_time_secs = 6,"), "{line}");
        assert!(line.contains(r#"chain_id = "42""#), "{line}");
    }

    #[test]
    fn inline_chain_refuses_a_fork_schedule_it_cannot_derive() {
        let fragment = format!("{COMMON_NETWORK_PARAMS}  fulu_fork_epoch: 2\n");
        let err = inline_chain(&fragment).unwrap_err();
        assert!(err.to_string().contains("fulu_fork_epoch"), "{err}");
    }

    #[test]
    fn the_chain_swap_replaces_only_the_first_line() {
        let block = format!("{CHAIN_FROM_SPEC}\n\n[pbs]\nport = {{{{ .Port }}}}");
        let out = with_inline_chain(&block, COMMON_NETWORK_PARAMS).unwrap();
        assert_eq!(
            out,
            format!("{PROBE_CHAIN}\n\n[pbs]\nport = {{{{ .Port }}}}")
        );
        // A block that does not open with the spec line is an error, not a no-op.
        assert!(with_inline_chain("[pbs]", COMMON_NETWORK_PARAMS).is_err());
    }
}
