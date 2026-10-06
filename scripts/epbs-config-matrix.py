#!/usr/bin/env python3
"""The config-matrix mode of scripts/run-epbs-sim.sh: one devnet, one [[mux]] per
group of validator keys, each group with different builder config values, and
every observed slot classified by its proposer's group.

The groups and why each one discriminates (buildoor's builder-API bid is pinned by
render-args to value V = 0.2 ETH plus execution_payment P = 0.2 ETH, P claimed
whatever the proposer advertised; its p2p bid is about 0.101 ETH; an empty devnet's
local block is worth about 0):

  g-default       nothing set: the validator client's own defaults apply
  g-boost0        builder_boost_factor = 0 at entry and key level: the local block
                  wins when the client applies the written boost
  g-boost0-entry  entry boost 0, key-level (p2p) boost 100: the CB bid loses when the
                  entry value governs builder-API bids, wins when the key-level does
  g-boost-high    builder_boost_factor = 1000: accepted and builder-built
  g-minbid-high   min_bid_eth = 1.0, above V + P: no bid via CB, and the key-level
                  min_bid (the same value) floors the p2p bid too
  g-unclamped     min_bid_eth = 0.3 between V and V + P, cap "unclamped": via CB only
                  when the client counts the execution payment
  g-cap0          min_bid_eth = 0.3, cap 0: the same bid at value alone, below the floor

Subcommands:
  render-config <cb-config.toml> <groups.json>
      rewrite the rendered single-mux CB config in place into one mux per group
      (the relay URL and the 64 keys come from the template's mux), and drop the
      [pbs] p2p projection keys so a group's key level is its own
  render-args <src.yaml> <dst.yaml> [--vc-param P]... [--buildoor-arg A]...
      copy a kurtosis args file, adding VC params to participant 1 and args to
      buildoor_params.extra_args
  stored <vc-km-url> <token-file> <groups.json> <km-dry-run.txt> <out.json>
      GET every key's builder_config; per group, print the doc cb-km wrote next to
      the doc the VC returns, and the fields the VC filled in or changed
  classify <bn-url> <first> <last> <groups.json> <cb.log> <bn.log> <vc.log> <out-dir>
      classify each slot in [first, last] by its proposer's group: via-cb (the
      block's bid is one CB returned for that slot), builder-other (a builder's bid
      CB did not return: p2p or a direct builder), self, missed; write slots.json
      and table.txt, print the table
  reclassify <out-dir> <groups.json> <cb.log>
      recompute the outcomes and table of a finished run from its slots.json and
      CB log, without the devnet
"""

import json
import os
import re
import sys
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from decimal import Decimal

BUILDER_INDEX_SELF_BUILD = 2**64 - 1
ANSI = re.compile(r"\x1b\[[0-9;]*m")
KEY_RE = re.compile(r"0x[0-9a-f]{96}")

# name, [[mux]] lines, relay lines, expected outcomes under the keymanager spec,
# what the group tests
GROUPS = [
    ("g-default", [], [], ["via-cb"], "VC defaults for every value cb-km leaves unset"),
    ("g-boost0", ["builder_boost_factor = 0"], [], ["self"], "boost 0 prefers the local block"),
    (
        "g-boost0-entry",
        ["builder_boost_factor = 0", "builder_boost_factor_p2p = 100"],
        [],
        ["builder-other", "self"],
        "entry boost 0, key-level boost 100: which one governs the CB bid",
    ),
    ("g-boost-high", ["builder_boost_factor = 1000"], [], ["via-cb"], "boost 1000 accepted"),
    ("g-minbid-high", ['min_bid_eth = "1.0"'], [], ["self"], "min_bid above the CB bid"),
    (
        "g-unclamped",
        ['min_bid_eth = "0.3"'],
        ['max_execution_payment_gwei = "unclamped"'],
        ["via-cb"],
        "min_bid between value and value + payment, cap unclamped",
    ),
    (
        "g-cap0",
        ['min_bid_eth = "0.3"'],
        ["max_execution_payment_gwei = 0"],
        ["self"],
        "the same floor, cap 0: value alone",
    ),
]


def get_json(url, token=None, timeout=10):
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + token} if token else {})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


# ---- render-config ------------------------------------------------------------
def render_config(path, groups_path):
    text = open(path).read()
    # the table headers, not their mentions in comments
    start = re.search(r"^\[\[mux\]\]\s*$", text, re.M).start()
    end = re.search(r"^\[logs\.stdout\]\s*$", text, re.M).start()
    # keep the comment block that introduces [logs.stdout]
    end = text.rfind("\n\n", start, end) + 2
    mux = text[start:end]
    keys = KEY_RE.findall(mux.split("[[mux.relays]]")[0])
    url = re.search(r'^url = "([^"]+)"', mux.split("[[mux.relays]]")[1], re.M).group(1)
    if len(keys) < len(GROUPS):
        sys.exit("render-config: {0} keys in the template's mux, need >= {1}".format(len(keys), len(GROUPS)))
    head = text[:start]
    # a group's key level must come from its own mux, not a [pbs] p2p default
    head = re.sub(r"^# projection-only \(KM tooling\).*\n(^#.*\n)*", "", head, flags=re.M)
    head = re.sub(r"^(min_bid_p2p_eth|builder_boost_factor_p2p) = .*\n", "", head, flags=re.M)
    groups, out = [], [head]
    for i, (name, mux_lines, relay_lines, expect, purpose) in enumerate(GROUPS):
        gkeys = keys[i :: len(GROUPS)]  # round robin: each group spread over the key list
        groups.append(
            {"name": name, "keys": gkeys, "mux": mux_lines, "relay": relay_lines, "expect": expect, "purpose": purpose}
        )
        out.append("# {0}: {1}\n[[mux]]\nid = \"{0}\"\n".format(name, purpose))
        out.append("validator_pubkeys = [\n" + "".join('    "{0}",\n'.format(k) for k in gkeys) + "]\n")
        out.extend(line + "\n" for line in mux_lines)
        out.append('\n[[mux.relays]]\nid = "buildoor-{0}"\nurl = "{1}"\n'.format(name, url))
        out.extend(line + "\n" for line in relay_lines)
        out.append("\n")
    out.append(text[end:])
    open(path, "w").write("".join(out))
    json.dump(groups, open(groups_path, "w"), indent=1)
    print("config-matrix: {0} keys in {1} muxes: {2}".format(
        len(keys), len(groups), ", ".join("{0}={1}".format(g["name"], len(g["keys"])) for g in groups)))


# ---- render-args --------------------------------------------------------------
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


# ---- stored -------------------------------------------------------------------
def written_docs(dry_run_path):
    docs = {}
    for line in open(dry_run_path, errors="replace"):
        m = re.match(r"would apply (0x[0-9a-f]{96}): (\{.*\})$", ANSI.sub("", line.strip()))
        if m:
            docs[m.group(1)] = json.loads(m.group(2))
    return docs


def flat(doc):
    """{path: value} with builder entries keyed by position."""
    out = {}
    for k, v in (doc or {}).items():
        if k == "builders":
            for i, e in enumerate(v or []):
                for ek, ev in e.items():
                    out["builders[{0}].{1}".format(i, ek)] = json.dumps(ev) if isinstance(ev, list) else ev
        else:
            out[k] = v
    return out


def stored(km, token_file, groups_path, dry_run_path, out_path):
    token = open(token_file).read().strip()
    groups = json.load(open(groups_path))
    written = written_docs(dry_run_path)
    result = {}
    for g in groups:
        result[g["name"]] = {}
        for key in g["keys"]:
            try:
                result[g["name"]][key] = {"stored": get_json(
                    "{0}/eth/v1/validator/{1}/builder_config".format(km, key), token)["data"]}
            except urllib.error.HTTPError as e:
                result[g["name"]][key] = {"error": "HTTP {0}: {1}".format(e.code, e.read()[:300].decode(errors="replace"))}
            except Exception as e:
                result[g["name"]][key] = {"error": "{0}: {1}".format(type(e).__name__, e)}
            result[g["name"]][key]["written"] = written.get(key)
    json.dump(result, open(out_path, "w"), indent=1)
    for g in groups:
        rows = result[g["name"]]
        key = g["keys"][0]
        row = rows[key]
        # the same shape on every key of the group?
        shapes = Counter(json.dumps(r.get("stored", r.get("error")), sort_keys=True) for r in rows.values())
        print("{0} ({1} keys, {2} distinct stored docs) e.g. {3}...".format(g["name"], len(rows), len(shapes), key[:18]))
        print("  written: {0}".format(json.dumps(row["written"], separators=(",", ":"))))
        if "error" in row:
            print("  GET:     {0}".format(row["error"]))
            continue
        print("  stored:  {0}".format(json.dumps(row["stored"], separators=(",", ":"))))
        w, s = flat(row["written"]), flat(row["stored"])
        filled = {k: s[k] for k in s if k not in w}
        changed = {k: (w[k], s[k]) for k in s if k in w and str(w[k]) != str(s[k])}
        dropped = [k for k in w if k not in s]
        print("  VC filled in:  {0}".format(filled or "-"))
        print("  VC changed:    {0}".format(changed or "-"))
        print("  VC dropped:    {0}".format(dropped or "-"))


# ---- classify -----------------------------------------------------------------
SLOT_RE = re.compile(r"\bslot[=:]\s*\"?(\d+)\b")
# per-slot evidence of the boost / selection a client used
BN_PATTERNS = re.compile(
    r"Selected (builder|engine|local|builder bid) block|Builder bid candidate|Ignoring invalid builder API bid"
    r"|Best p2p bid below configured minimum|Unable to request builder API bids"  # lodestar
    r"|Chose payload bid|Builder bids:"  # prysm
    r"|Producing Gloas block|Skipping gossip bid|Skipping bid from a builder|no proposer preferences"  # lighthouse
)
VC_PATTERNS = re.compile(r"Producing block|Produced block|builderBoostFactor|builder_boost_factor|Requesting unsigned block"
                         r"|Builder config|Published block|Successfully published block")


def lines_by_slot(path, pattern):
    by = defaultdict(list)
    try:
        for raw in open(path, errors="replace"):
            line = ANSI.sub("", raw.rstrip("\n"))
            if not pattern.search(line):
                continue
            m = SLOT_RE.search(line)
            if m:
                by[int(m.group(1))].append(line[:600])
    except FileNotFoundError:
        pass
    return by


def eth_to_gwei(eth):
    return int(Decimal(eth) * 10**9)


def cb_evidence(path):
    """CB's log: per slot, the bids it returned to the beacon node as (value gwei,
    execution_payment gwei, block_hash); per validator, its bid requests and the
    max_execution_payment its beacon node advertised in builder preferences."""
    bids = defaultdict(list)
    requests = Counter()
    prefs = defaultdict(Counter)
    for raw in open(path, errors="replace"):
        line = ANSI.sub("", raw)
        slot = re.search(r"\bslot=(\d+)", line)
        val = re.search(r"\bvalidator=(0x[0-9a-f]{96})", line)
        if "received header" in line or "received new header" in line:
            h = re.search(r"block_hash=(0x[0-9a-fA-F]+(?:\u2026[0-9a-fA-F]+)?)", line)
            v = re.search(r'trustless_bid_eth="([0-9.]+)"', line)
            p = re.search(r'execution_payment_eth="([0-9.]+)"', line)
            if slot and h and v and p:
                bid = (eth_to_gwei(v.group(1)), eth_to_gwei(p.group(1)), h.group(1).lower())
                if bid not in bids[int(slot.group(1))]:
                    bids[int(slot.group(1))].append(bid)
        elif "new request" in line and val:
            if "execution_payload_bid" in line:
                requests[val.group(1)] += 1
            elif "builder_preferences" in line:
                m = re.search(r"\bmax_execution_payment=(\d+)", line)
                prefs[val.group(1)][m.group(1) if m else "?"] += 1
    return bids, requests, prefs


def abbrev(h):
    return h[:6] + "\u2026" + h[-4:]


def outcome(bid, cb_bids):
    """self, via-cb or builder-other for a block's bid. via-cb needs the hash AND
    the value and execution_payment of a bid CB returned for the slot: buildoor
    bids the same payload over p2p at another value, so a hash alone also
    matches its p2p bid."""
    if int(bid["builder_index"]) == BUILDER_INDEX_SELF_BUILD:
        return "self"
    h, v, p = bid["block_hash"].lower(), int(bid["value"]), int(bid["execution_payment"])
    for cv, cp, ch in cb_bids:
        if ch in (h, abbrev(h)) and (cv, cp) == (v, p):
            return "via-cb"
    return "builder-other"


def classify(argv):
    bn, first, last, groups_path, cb_log, bn_log, vc_log, out_dir = argv
    first, last = int(first), int(last)
    groups = json.load(open(groups_path))
    group_of = {k: g["name"] for g in groups for k in g["keys"]}
    validators = {int(v["index"]): v["validator"]["pubkey"]
                  for v in get_json(bn + "/eth/v1/beacon/states/head/validators", timeout=30)["data"]}
    duties, duty_errors = {}, []
    for epoch in sorted({s // slots_per_epoch(bn) for s in range(first, last + 1)}):
        try:
            for d in get_json("{0}/eth/v1/validator/duties/proposer/{1}".format(bn, epoch))["data"]:
                duties[int(d["slot"])] = d["pubkey"]
        except Exception as e:
            duty_errors.append("epoch {0}: {1}".format(epoch, e))
    cb_bids, cb_requests, cb_prefs = cb_evidence(cb_log)
    bn_lines = lines_by_slot(bn_log, BN_PATTERNS)
    vc_lines = lines_by_slot(vc_log, VC_PATTERNS)
    slots, errors = [], []
    for slot in range(first, last + 1):
        row = {"slot": slot}
        try:
            with urllib.request.urlopen("{0}/eth/v2/beacon/blocks/{1}".format(bn, slot), timeout=10) as r:
                msg = json.load(r)["data"]["message"]
            pub = validators.get(int(msg["proposer_index"]))
            bid = msg["body"]["signed_execution_payload_bid"]["message"]
            row.update(proposer=pub, bid={k: bid.get(k) for k in ("builder_index", "value", "execution_payment", "block_hash")})
            row["outcome"] = outcome(bid, cb_bids.get(slot, []))
        except urllib.error.HTTPError as e:
            if e.code != 404:
                errors.append("slot {0}: HTTP {1}".format(slot, e.code))
                continue
            row.update(proposer=duties.get(slot), outcome="missed")
        except Exception as e:
            errors.append("slot {0}: {1}: {2}".format(slot, type(e).__name__, e))
            continue
        row["group"] = group_of.get(row["proposer"], "?")
        row["cb_bids"] = cb_bids.get(slot, [])
        row["bn_lines"] = bn_lines.get(slot, [])
        row["vc_lines"] = vc_lines.get(slot, [])
        slots.append(row)

    header = "window {0}..{1}: {2} slots classified, {3} not classified".format(first, last, len(slots), len(errors))
    short = report(slots, groups, cb_requests, cb_prefs, header, duty_errors + errors, out_dir)
    return 0 if not errors and not short else 1


def report(slots, groups, cb_requests, cb_prefs, header, notes, out_dir):
    """Write table.txt, slots.json and evidence-per-group.txt; return the groups
    with fewer than CM_MIN_PROPOSALS blocks."""
    outcomes = ["via-cb", "builder-other", "self", "missed"]
    table = defaultdict(Counter)
    other = defaultdict(Counter)  # group -> value/execution_payment of the builder-other bids
    for row in slots:
        table[row["group"]][row["outcome"]] += 1
        if row["outcome"] == "builder-other":
            other[row["group"]]["{0}/{1}".format(row["bid"]["value"], row["bid"]["execution_payment"])] += 1
    lines = [header]
    hdr = "{0:<15} {1:>4} {2:>9} {3:>7} {4:>13} {5:>5} {6:>6}  {7:<26} {8}".format(
        "group", "keys", "proposals", "via-cb", "builder-other", "self", "missed", "spec expectation", "observed matches spec")
    lines += [hdr, "-" * len(hdr)]
    for g in groups + [{"name": "?", "keys": [], "expect": []}]:
        c = table.get(g["name"])
        if c is None and g["name"] == "?":
            continue
        c = c or Counter()
        proposed = sum(c[o] for o in outcomes if o != "missed")
        seen = {o for o in outcomes[:3] if c[o]}
        match = "n/a (no block)" if not seen else ("yes" if seen <= set(g["expect"]) else "NO")
        lines.append("{0:<15} {1:>4} {2:>9} {3:>7} {4:>13} {5:>5} {6:>6}  {7:<26} {8}".format(
            g["name"], len(g["keys"]), proposed, c["via-cb"], c["builder-other"], c["self"], c["missed"],
            " or ".join(g["expect"]) or "-", match))
    if other:
        lines.append("builder-other bids (value/execution_payment gwei): " + "; ".join(
            "{0} {1}".format(g, dict(c)) for g, c in other.items()))
    lines.append("")
    lines.append("CB per group: bid requests from the BN, max_execution_payment the BN advertised (builder preferences)")
    for g in groups:
        reqs = sum(cb_requests[k] for k in g["keys"])
        prefs = Counter()
        for k in g["keys"]:
            prefs.update(cb_prefs.get(k, {}))
        lines.append("  {0:<15} requests={1:<5} preferences max_execution_payment={2}".format(
            g["name"], reqs, dict(prefs) or "none seen"))
    for e in notes:
        lines.append("  note: " + e)
    need = int(os.environ.get("CM_MIN_PROPOSALS", "0"))
    short = [g["name"] for g in groups
             if sum(table[g["name"]][o] for o in ("via-cb", "builder-other", "self")) < need]
    if short:
        lines.append("INCOMPLETE: fewer than {0} blocks proposed by {1}".format(need, ", ".join(short)))
    out = "\n".join(lines)
    print(out)
    open(out_dir + "/table.txt", "w").write(out + "\n")
    json.dump(slots, open(out_dir + "/slots.json", "w"), indent=1)
    # two proposals per group with the client's own lines for those slots
    ev = []
    for g in groups:
        rows = [r for r in slots if r["group"] == g["name"] and r["outcome"] != "missed"]
        ev.append("== {0}: {1} proposals".format(g["name"], len(rows)))
        for r in rows[:2]:
            ev.append("  slot {0}: {1} bid={2} cb_bids={3}".format(r["slot"], r["outcome"], r["bid"], r["cb_bids"]))
            ev += ["    BN: " + l for l in r["bn_lines"][:6]]
            ev += ["    VC: " + l for l in r["vc_lines"][:3]]
    open(out_dir + "/evidence-per-group.txt", "w").write("\n".join(ev) + "\n")
    return short


def reclassify(out_dir, groups_path, cb_log):
    """Recompute every outcome from a finished run's slots.json and CB log, without
    the devnet: the blocks' bids are in slots.json."""
    groups = json.load(open(groups_path))
    slots = json.load(open(out_dir + "/slots.json"))
    cb_bids, cb_requests, cb_prefs = cb_evidence(cb_log)
    for row in slots:
        row["cb_bids"] = cb_bids.get(row["slot"], [])
        if row["outcome"] != "missed":
            row["outcome"] = outcome(row["bid"], row["cb_bids"])
    first, last = slots[0]["slot"], slots[-1]["slot"]
    header = "window {0}..{1}: {2} slots classified (reclassified from slots.json)".format(first, last, len(slots))
    return 1 if report(slots, groups, cb_requests, cb_prefs, header, [], out_dir) else 0


def slots_per_epoch(bn, _cache={}):
    if bn not in _cache:
        _cache[bn] = int(get_json(bn + "/eth/v1/config/spec")["data"]["SLOTS_PER_EPOCH"])
    return _cache[bn]


def main():
    cmd, argv = sys.argv[1], sys.argv[2:]
    if cmd == "render-config":
        render_config(*argv)
    elif cmd == "render-args":
        render_args(argv)
    elif cmd == "stored":
        stored(*argv)
    elif cmd == "classify":
        sys.exit(classify(argv))
    elif cmd == "reclassify":
        sys.exit(reclassify(*argv))
    else:
        sys.exit("unknown subcommand " + cmd)


main()
