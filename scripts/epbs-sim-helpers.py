#!/usr/bin/env python3
"""Helpers for scripts/run-epbs-sim.sh.

Subcommands:
  render-args <src.yaml> <dst.yaml> [--vc-param P]... [--buildoor-arg A]...
      copy a kurtosis args file, adding VC params to participant 1 and args to
      buildoor_params.extra_args
  via-cb <bn-url> <first> <last> <cb.log>
      print via_cb=<slots> and builder_other=<slots>: the builder-built blocks in
      [first, last] whose bid is one CB returned for that slot, and the rest
"""

import json
import re
import sys
import urllib.error
import urllib.request
from collections import defaultdict
from decimal import Decimal

BUILDER_INDEX_SELF_BUILD = 2**64 - 1
ANSI = re.compile(r"\x1b\[[0-9;]*m")


# ---- render-args ---------------------------------------------------------------
def insert_list_items(lines, header_idx, items):
    """Insert '- item' lines under the list whose header is lines[header_idx]."""
    nxt = lines[header_idx + 1] if header_idx + 1 < len(lines) else ""
    m = re.match(r"(\s*)-\s", nxt)
    indent = m.group(1) if m else re.match(r"(\s*)", lines[header_idx]).group(1) + "  "
    for item in reversed(items):
        lines.insert(header_idx + 1, "{0}- {1}\n".format(indent, item))


def render_args(argv):
    src, dst, rest = argv[0], argv[1], argv[2:]
    vc, bo = [], []
    while rest:
        flag, val, rest = rest[0], rest[1], rest[2:]
        {"--vc-param": vc, "--buildoor-arg": bo}[flag].append(val)
    lines = open(src).read().splitlines(keepends=True)
    if vc:
        # participant 1: the lines before the second "- el_type" / "- cl_type" item
        first = next(i for i, l in enumerate(lines) if re.match(r"\s*-\s+(el|cl)_type:", l))
        nxt = next((i for i in range(first + 1, len(lines)) if re.match(r"\s*-\s+(el|cl)_type:", lines[i])
                    or re.match(r"\S", lines[i])), len(lines))
        h = next((i for i in range(first, nxt) if re.match(r"\s*vc_extra_params:\s*$", lines[i])), None)
        if h is None:
            anchor = next((i for i in range(first, nxt) if re.match(r"\s*vc_log_level:", lines[i])), None)
            if anchor is None:
                sys.exit("render-args: participant 1 has neither vc_extra_params nor vc_log_level")
            indent = re.match(r"(\s*)", lines[anchor]).group(1)
            lines.insert(anchor + 1, indent + "vc_extra_params:\n")
            h = anchor + 1
        insert_list_items(lines, h, vc)
    if bo:
        b = next((i for i, l in enumerate(lines) if re.match(r"buildoor_params:\s*$", l)), None)
        e = next((i for i in range(b + 1, len(lines)) if re.match(r"\s+extra_args:\s*$", lines[i])), None) if b is not None else None
        if e is None:
            sys.exit("render-args: no buildoor_params.extra_args list")
        insert_list_items(lines, e, bo)
    open(dst, "w").write("".join(lines))


# ---- via-cb --------------------------------------------------------------------
def eth_to_gwei(eth):
    return int(Decimal(eth) * 10**9)


def cb_bids(path):
    """CB's log: per slot, the bids it returned to the beacon node as (value gwei,
    execution_payment gwei, block_hash)."""
    bids = defaultdict(list)
    for raw in open(path, errors="replace"):
        line = ANSI.sub("", raw)
        if "received header" not in line and "received new header" not in line:
            continue
        slot = re.search(r"\bslot=(\d+)", line)
        h = re.search(r"block_hash=(0x[0-9a-fA-F]+(?:…[0-9a-fA-F]+)?)", line)
        v = re.search(r'trustless_bid_eth="([0-9.]+)"', line)
        p = re.search(r'execution_payment_eth="([0-9.]+)"', line)
        if slot and h and v and p:
            bid = (eth_to_gwei(v.group(1)), eth_to_gwei(p.group(1)), h.group(1).lower())
            if bid not in bids[int(slot.group(1))]:
                bids[int(slot.group(1))].append(bid)
    return bids


def abbrev(h):
    return h[:6] + "…" + h[-4:]


def outcome(bid, slot_bids):
    """self, via-cb or builder-other for a block's bid. via-cb needs the hash AND
    the value and execution_payment of a bid CB returned for the slot: buildoor
    bids the same payload over p2p at another value, so a hash alone also
    matches its p2p bid."""
    if int(bid["builder_index"]) == BUILDER_INDEX_SELF_BUILD:
        return "self"
    h, v, p = bid["block_hash"].lower(), int(bid["value"]), int(bid["execution_payment"])
    for cv, cp, ch in slot_bids:
        if ch in (h, abbrev(h)) and (cv, cp) == (v, p):
            return "via-cb"
    return "builder-other"


def via_cb(bn, first, last, cb_log):
    bids = cb_bids(cb_log)
    found = {"via-cb": [], "builder-other": []}
    for slot in range(int(first), int(last) + 1):
        try:
            with urllib.request.urlopen("{0}/eth/v2/beacon/blocks/{1}".format(bn, slot), timeout=10) as r:
                msg = json.load(r)["data"]["message"]
        except urllib.error.HTTPError as e:
            if e.code == 404:
                continue
            raise
        kind = outcome(msg["body"]["signed_execution_payload_bid"]["message"], bids.get(slot, []))
        if kind in found:
            found[kind].append(str(slot))
    print("via_cb=" + ",".join(found["via-cb"]))
    print("builder_other=" + ",".join(found["builder-other"]))


def main():
    cmd, argv = sys.argv[1], sys.argv[2:]
    if cmd == "render-args":
        render_args(argv)
    elif cmd == "via-cb":
        via_cb(*argv)
    else:
        sys.exit("unknown subcommand " + cmd)


main()
