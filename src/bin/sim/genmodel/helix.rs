//! The helix relay YAML block — a verbatim port of the Python
//! `build_helix_relay_config()` output (`scripts/generate_kurtosis_configs.py`).
//!
//! This block is byte-IDENTICAL across all 6 scenarios (verified by diff), so it
//! is a single `const`. The `{{ .POSTGRES_* }}`, `{{ .BEACON_URI }}`, and
//! `{{ .BLOCKSIM_URI }}` are runtime holes filled by the ethereum-package at
//! launch — they stay as literal text here.
//!
//! The ~40 lines of comments are the expensive, hard-won knowledge (the
//! `network_config` removal, the binary-verified 10-field `CoresConfig`) — PRESERVE
//! them verbatim. Do not "clean them up".

/// The helix relay config, de-indented (no leading 4-space block-scalar indent).
/// `scenario::args_file` re-indents it 4 spaces when embedding it as the
/// `helix_relay_config: |` block scalar. No trailing newline (matches Python).
pub const HELIX_RELAY_CONFIG: &str = r#"instance_id: "helix-kurtosis-test"

# NOTE: no network/genesis section. Current helix-relay:main removed the old
# `network_config: !Custom {dir_path, genesis_validator_root, genesis_time}`
# field entirely; the relay now fetches the chain spec + genesis from the
# beacon node at startup (GET eth/v1/config/spec + eth/v1/beacon/genesis, see
# beacon_client.get_chain_info -> main.rs load_chain_info). A `!Custom` YAML
# tag on that now-unknown key makes serde_yaml panic with
# "untagged and internally tagged enums do not support enum input".

postgres:
  hostname: "{{ .POSTGRES_HOST_NAME }}"
  port: {{ .POSTGRES_PORT }}
  db_name: "{{ .POSTGRES_DB }}"
  user: "{{ .POSTGRES_USER }}"
  password: "{{ .POSTGRES_PASS }}"
  region: 0
  region_name: "LOCAL"

beacon_clients:
  - url: "{{ .BEACON_URI }}"

gossip_payload_on_header: false

simulators:
  - url: "{{ .BLOCKSIM_URI }}"
    namespace: flashbots
    is_merging_simulator: false
    max_concurrent_tasks: 32

router_config:
  enabled_routes:
    - route: GetValidators
    - route: SubmitBlock
    - route: GetTopBid
    - route: GetHeader
      rate_limit:
        replenish_ms: 50
        burst_size: 20
    - route: GetPayload
    # The builder-spec v2 proposer route (submitBlindedBlockV2). REQUIRED for
    # any CL that submits via v2 -- prysm does. Without it helix 404s
    # /eth/v2/builder/blinded_blocks, CB (correctly) refuses to downgrade to v1
    # (v2 semantics: the relay publishes the block, so a v1 payload would be
    # silently dropped), returns 502, and EVERY builder block the proposer
    # chose is lost. That read as "prysm can't do MEV" until the route list was
    # checked -- see .agent/SWEEP-BACKLOG.md (Law 7 first dividend).
    - route: GetPayloadV2
    - route: HeaderStream
    - route: RegisterValidators
    - route: Status
    - route: ProposerPayloadDelivered
    - route: BuilderBidsReceived
    - route: ValidatorRegistration
  shutdown_delay_ms: 12000

timing_game_config:
  max_header_delay_ms: 400
  latest_header_delay_ms_in_slot: 1500
  default_client_latency_ms: 50

target_get_payload_propagation_duration_ms: 500

is_submission_instance: true
is_registration_instance: true

admin_token: "test_admin_token"

logging:
  type: Console

# CoresConfig (helix_common::config CoresConfig, 10 fields in current
# :main). The old block used `sub_workers: [0]` and omitted the per-tile
# core assignments; current :main removed sub_workers and added
# decoder/simulator/top_bid/data_gatherer/block_merging/housekeeper. The
# outer RelayConfigExt flattens RelayConfig, so a wrong/missing cores field
# surfaces as a top-level "missing field `decoder`" / "invalid type ...
# expected usize|sequence" serde error. Verified against the binary:
# `decoder` is Vec<usize> ([0]); the other five new tile fields are usize.
cores:
  auctioneer: 0
  tokio: [0]
  reg_workers: [0]
  tcp_bid_submissions_tile: 2
  decoder: [0]
  simulator: 0
  top_bid: 0
  data_gatherer: 0
  block_merging: 0
  housekeeper: 0

is_local_dev: false"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genmodel::extract_block_scalar;

    /// The const must equal the `helix_relay_config: |` block of the cb-basic
    /// golden, de-indented 4 spaces. (The block is identical in every golden, so
    /// one is enough.)
    #[test]
    fn helix_const_matches_golden_block() {
        let golden = crate::genmodel::golden("cb-basic");
        let block = extract_block_scalar(golden, "helix_relay_config");
        assert_eq!(HELIX_RELAY_CONFIG, block);
    }
}
