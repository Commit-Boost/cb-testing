#!/usr/bin/env python3
"""Generate Kurtosis YAML configs for Commit-Boost testing scenarios."""

import argparse
import json
import os
import sys


SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
KEYS_CONFIGS_DIR = os.path.join(SCRIPT_DIR, "../keys")

# ---------------------------------------------------------------------------
# Shared YAML fragments
# ---------------------------------------------------------------------------

COMMON_PARTICIPANTS = """\
participants:
  - el_type: geth
    cl_type: lighthouse"""

COMMON_ADDITIONAL_SERVICES = """\
additional_services:
  - dora
  - spamoor
  - prometheus
  - assertoor"""

COMMON_NETWORK_PARAMS = (
    "network_params:\n"
    '  network: kurtosis\n'
    '  network_id: "3151908"\n'
    '  deposit_contract_address: "0x00000000219ab540356cBB839Cbe05303d7705Fa"\n'
    "  seconds_per_slot: 12\n"
    "  slot_duration_ms: 12000\n"
    "  num_validator_keys_per_node: 128\n"
    "  preregistered_validator_keys_mnemonic:\n"
    '    "giant issue aisle success illegal bike spike\n'
    "    question tent bar rely arctic volcano long crawl hungry vocal artwork sniff fantasy\n"
    '    very lucky have athlete"'
)

ASSERTOOR_PARAMS = (
    "assertoor_params:\n"
    '  run_stability_check: false\n'
    '  run_block_proposal_check: false\n'
    "  tests:\n"
    '    file: "http://host.docker.internal:8888/assertoor/cb-mev-pipeline.yaml"'
)

MUX_NETWORK_PARAMS = (
    "network_params:\n"
    '  network: kurtosis\n'
    '  network_id: "3151908"\n'
    '  deposit_contract_address: "0x00000000219ab540356cBB839Cbe05303d7705Fa"\n'
    "  seconds_per_slot: 12\n"
    "  slot_duration_ms: 12000\n"
    "  num_validator_keys_per_node: 256\n"
    "  preregistered_validator_keys_mnemonic:\n"
    '    "giant issue aisle success illegal bike spike\n'
    "    question tent bar rely arctic volcano long crawl hungry vocal artwork sniff fantasy\n"
    '    very lucky have athlete"'
)
# ---------------------------------------------------------------------------

def load_pubkeys(filename):
    path = os.path.join(KEYS_CONFIGS_DIR, filename)
    if not os.path.isfile(path):
        print(f"Error: missing pubkey file {path}", file=sys.stderr)
        sys.exit(1)
    with open(path, "r") as f:
        return json.load(f)


def format_pubkey_list(pubkeys):
    """Return a multiline list literal with 4-space entry indentation.

    When placed inside a YAML literal block that is itself indented 4 spaces,
    the entries end up at 8 spaces total — matching the ground truth.
    """
    lines = ["["]
    for i, pk in enumerate(pubkeys):
        comma = "" if i == len(pubkeys) - 1 else ","
        lines.append(f'    "{pk}"{comma}')
    lines.append("]")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# TOML builders (raw; get indented 4 spaces by build_mev_params)
# ---------------------------------------------------------------------------

def build_cb_toml_basic(timeout_get_header_ms, timeout_get_payload_ms,
                        extra_pbs_lines=None, per_relay_lines=None):
    lines = [
        'chain = { genesis_time_secs = {{ .Timestamp }}, path = "{{ .Network }}" }',
        "",
        "[pbs]",
        'host = "0.0.0.0"',
        "port = {{ .Port }}",
        f"timeout_get_header_ms = {timeout_get_header_ms}",
        f"timeout_get_payload_ms = {timeout_get_payload_ms}",
        "late_in_slot_time_ms = 2000",
    ]

    if extra_pbs_lines:
        # Insert after port (idx 4), before timeouts (idx 5)
        insert_idx = 5
        for line in extra_pbs_lines:
            lines.insert(insert_idx, line)
            insert_idx += 1

    lines.append("")
    lines.append("{{ range $index, $relay := .Relays }}")
    lines.append("[[relays]]")
    lines.append('id = "mev_relay_{{$index}}"')
    lines.append('url = "{{ $relay }}"')

    if per_relay_lines:
        for line in per_relay_lines:
            lines.append(line)

    lines.append("{{- end }}")
    lines.append("")
    lines.append("[logs.stdout]")
    lines.append('level = "debug"')
    lines.append("")
    lines.append("[logs.file]")
    lines.append("enabled = false")

    return "\n".join(lines)


def build_cb_toml_mux(pubkeys_node0, pubkeys_node1):
    node0_list = format_pubkey_list(pubkeys_node0)
    node1_list = format_pubkey_list(pubkeys_node1)

    lines = [
        'chain = { genesis_time_secs = {{ .Timestamp }}, path = "{{ .Network }}" }',
        "",
        "[pbs]",
        'host = "0.0.0.0"',
        "port = {{ .Port }}",
        "timeout_get_header_ms = 950",
        "timeout_get_payload_ms = 4000",
        "late_in_slot_time_ms = 2000",
        "",
        "{{ range $index, $relay := .Relays }}",
        "[[relays]]",
        'id = "mev_relay_{{$index}}"',
        'url = "{{ $relay }}"',
        "{{- end }}",
        "",
        "[[mux]]",
        'id = "node_0_to_helix"',
        f"validator_pubkeys = {node0_list}",
        "timeout_get_header_ms = 900",
        "[[mux.relays]]",
        'id = "mux_helix"',
        'url = "{{ index .Relays 0 }}"',
        "",
        "[[mux]]",
        'id = "node_1_to_flashbots"',
        f"validator_pubkeys = {node1_list}",
        "timeout_get_header_ms = 900",
        "[[mux.relays]]",
        'id = "mux_flashbots"',
        'url = "{{ index .Relays 1 }}"',
        "",
        "[logs.stdout]",
        'level = "debug"',
        "",
        "[logs.file]",
        "enabled = false",
    ]

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# MEV params builder
# ---------------------------------------------------------------------------

def build_mev_params(relays, images, toml_block):
    lines = ["mev_params:"]

    if isinstance(relays, list):
        lines.append("  mev_relay:")
        for r in relays:
            lines.append(f"    - {r}")
    else:
        lines.append(f"  mev_relay: {relays}")

    lines.append("  mev_sidecar: commit-boost")
    lines.append("  mev_builder: flashbots")
    lines.append("")

    for key, val in images.items():
        lines.append(f"  {key}: {val}")

    lines.append("")
    lines.append("  commit_boost_config: |")
    # Indent every non-empty TOML line by 4 spaces; keep blanks truly empty
    for line in toml_block.splitlines():
        if line.strip():
            lines.append(f"    {line}")
        else:
            lines.append("")
    # NB: no trailing empty line here — that is handled by the caller's join

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Scenario generators
# ---------------------------------------------------------------------------

def generate_basic():
    comment = (
        "# cb-basic: Single relay (helix) with default Commit-Boost config.\n"
        "#\n"
        "# Tests the core MEV pipeline through Commit-Boost with a single Helix\n"
        "# relay as the only relay endpoint."
    )
    images = {
        "helix_relay_image": "helix-relay:kurtosis",
        "mev_boost_image": "commit-boost/pbs:kurtosis",
        "mev_builder_cl_image": "sigp/lighthouse:latest",
        "mev_builder_image": "ethpandaops/reth-rbuilder:develop",
    }
    toml = build_cb_toml_basic(950, 4000)
    mev_params = build_mev_params("helix", images, toml)
    return "\n\n".join([
        comment,
        COMMON_PARTICIPANTS,
        COMMON_ADDITIONAL_SERVICES,
        "mev_type: custom",
        mev_params,
        COMMON_NETWORK_PARAMS,
        ASSERTOOR_PARAMS,
    ]) + "\n"


def generate_multiple_relays():
    comment = (
        "# cb-multiple-relays: Two relays (helix + flashbots) behind a single\n"
        "# Commit-Boost sidecar.\n"
        "#\n"
        "# Tests that CB correctly routes get_header requests to both relays,\n"
        "# aggregating responses and selecting the best bid."
    )
    images = {
        "helix_relay_image": "helix-relay:kurtosis",
        "mev_relay_image": "ethpandaops/mev-boost-relay:main",
        "mev_boost_image": "commit-boost/pbs:kurtosis",
        "mev_builder_cl_image": "sigp/lighthouse:latest",
        "mev_builder_image": "ethpandaops/reth-rbuilder:develop",
    }
    toml = build_cb_toml_basic(950, 4000)
    mev_params = build_mev_params(["helix", "flashbots"], images, toml)
    return "\n\n".join([
        comment,
        COMMON_PARTICIPANTS,
        COMMON_ADDITIONAL_SERVICES,
        "mev_type: custom",
        mev_params,
        COMMON_NETWORK_PARAMS,
    ]) + "\n"


def generate_skip_sigverify():
    comment = (
        "# cb-skip-sigverify: Signature verification disabled for header responses.\n"
        "#\n"
        "# Tests the CB fast path where BLS verification is skipped. This trades\n"
        "# correctness for speed — useful to verify that the path exists and is\n"
        "# reachable under load."
    )
    images = {
        "helix_relay_image": "helix-relay:kurtosis",
        "mev_boost_image": "commit-boost/pbs:kurtosis",
        "mev_builder_cl_image": "sigp/lighthouse:latest",
        "mev_builder_image": "ethpandaops/reth-rbuilder:develop",
    }
    toml = build_cb_toml_basic(950, 4000, extra_pbs_lines=["skip_sigverify = true"])
    mev_params = build_mev_params("helix", images, toml)
    return "\n\n".join([
        comment,
        COMMON_PARTICIPANTS,
        COMMON_ADDITIONAL_SERVICES,
        "mev_type: custom",
        mev_params,
        COMMON_NETWORK_PARAMS,
    ]) + "\n"


def generate_timing_games():
    comment = (
        "# cb-timing-games: Aggressive timing game configuration.\n"
        "#\n"
        "# Tests CB's ability to orchestrate repeated get_header polls with\n"
        "# short timeouts in order to arrive at the best bid as late as possible\n"
        "# in the slot. Per-relay timing overrides are enabled for all relays."
    )
    images = {
        "helix_relay_image": "helix-relay:kurtosis",
        "mev_relay_image": "ethpandaops/mev-boost-relay:main",
        "mev_boost_image": "commit-boost/pbs:kurtosis",
        "mev_builder_cl_image": "sigp/lighthouse:latest",
        "mev_builder_image": "ethpandaops/reth-rbuilder:develop",
    }
    toml = build_cb_toml_basic(
        400,
        2000,
        per_relay_lines=[
            "enable_timing_games = true",
            "target_first_request_ms = 100",
            "frequency_get_header_ms = 200",
        ],
    )
    mev_params = build_mev_params(["helix", "flashbots"], images, toml)
    return "\n\n".join([
        comment,
        COMMON_PARTICIPANTS,
        COMMON_ADDITIONAL_SERVICES,
        "mev_type: custom",
        mev_params,
        COMMON_NETWORK_PARAMS,
    ]) + "\n"


def generate_extra_validation():
    comment = (
        "# cb-extra-validation: Enable extra validation of get_header responses\n"
        "# via a local execution layer client.\n"
        "#\n"
        "# Tests that CB will RPC-call the execution client to verify block\n"
        "# parameters before returning a header to the beacon node."
    )
    images = {
        "helix_relay_image": "helix-relay:kurtosis",
        "mev_boost_image": "commit-boost/pbs:kurtosis",
        "mev_builder_cl_image": "sigp/lighthouse:latest",
        "mev_builder_image": "ethpandaops/reth-rbuilder:develop",
    }
    toml = build_cb_toml_basic(
        950,
        4000,
        extra_pbs_lines=[
            "extra_validation_enabled = true",
            'rpc_url = "http://el-1-geth-lighthouse:8545"',
        ],
    )
    mev_params = build_mev_params("helix", images, toml)
    return "\n\n".join([
        comment,
        COMMON_PARTICIPANTS,
        COMMON_ADDITIONAL_SERVICES,
        "mev_type: custom",
        mev_params,
        COMMON_NETWORK_PARAMS,
    ]) + "\n"


def generate_mux(pubkeys_node0, pubkeys_node1):
    comment = (
        "# cb-mux: Multiplexed relay routing per validator node.\n"
        "#\n"
        "# Routes all 128 validators from node-0 exclusively to the Helix relay and\n"
        "# all 128 validators from node-1 exclusively to the Flashbots relay.\n"
        "# This tests CB's ability to partition the validator set and apply\n"
        "# per-mux timeout and relay configurations."
    )
    images = {
        "helix_relay_image": "helix-relay:kurtosis",
        "mev_relay_image": "ethpandaops/mev-boost-relay:main",
        "mev_boost_image": "commit-boost/pbs:kurtosis",
        "mev_builder_cl_image": "sigp/lighthouse:latest",
        "mev_builder_image": "ethpandaops/reth-rbuilder:develop",
    }
    toml = build_cb_toml_mux(pubkeys_node0, pubkeys_node1)
    mev_params = build_mev_params(["helix", "flashbots"], images, toml)
    return "\n\n".join([
        comment,
        COMMON_PARTICIPANTS,
        COMMON_ADDITIONAL_SERVICES,
        "mev_type: custom",
        mev_params,
        MUX_NETWORK_PARAMS,
    ]) + "\n"


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Generate Kurtosis YAML configs for Commit-Boost testing."
    )
    parser.add_argument(
        "--output-dir",
        default="configs/generated",
        help="Directory to write generated YAML configs (default: kurtosis-configs).",
    )
    args = parser.parse_args()

    output_dir = os.path.abspath(args.output_dir)
    os.makedirs(output_dir, exist_ok=True)

    pubkeys_node0 = load_pubkeys("node-0-pubkeys.json")
    pubkeys_node1 = load_pubkeys("node-1-pubkeys.json")

    scenarios = {
        "cb-basic.yml": generate_basic(),
        "cb-multiple-relays.yml": generate_multiple_relays(),
        "cb-skip-sigverify.yml": generate_skip_sigverify(),
        "cb-timing-games.yml": generate_timing_games(),
        "cb-extra-validation.yml": generate_extra_validation(),
        "cb-mux.yml": generate_mux(pubkeys_node0, pubkeys_node1),
    }

    for filename, content in scenarios.items():
        path = os.path.join(output_dir, filename)
        with open(path, "w") as f:
            f.write(content)
        print(f"Generated {path}")


if __name__ == "__main__":
    main()
