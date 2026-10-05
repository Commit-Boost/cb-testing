#!/usr/bin/env python3
"""Classify gloas blocks as builder-built or self-built, from the beacon API.

A gloas block body carries the signed_execution_payload_bid the proposer
committed to. The bid's builder_index says who builds the payload:
BUILDER_INDEX_SELF_BUILD (2**64 - 1) is the proposer building its own, and any
other index names a builder in the on-chain builder registry.

The bid's value does not mark a builder-built block. A builder can pay the
proposer entirely through execution_payment (the trusted payment the
proposer's max_execution_payment allows) and leave value at 0; buildoor does
exactly that whenever the cap covers its whole bid.

Usage:
  epbs-blocks.py classify <beacon-url> <first-slot> <last-slot>
      stdout: three lines the sim asserts parse, slots comma-separated:
        builder=<slots>        builder_index != BUILDER_INDEX_SELF_BUILD
        self=<slots>           builder_index == BUILDER_INDEX_SELF_BUILD
        payment_only=<slots>   the builder-built slots with value == 0 and
                               execution_payment > 0
      A 404 is an empty slot. Any other failure is reported on stderr with its
      slot and cause and the slot lands in no list, so an unreachable beacon
      node never reads as "nothing was built".
  epbs-blocks.py self-test
      Runs the classifier against both sides of each boundary; exit 0 = pass.
"""

import json
import sys
import urllib.error
import urllib.request

BUILDER_INDEX_SELF_BUILD = 2**64 - 1


def bid_message(block):
    """The signed_execution_payload_bid message of a /eth/v2/beacon/blocks response."""
    body = block["data"]["message"]["body"]
    if "signed_execution_payload_bid" not in body:
        raise ValueError("block body has no signed_execution_payload_bid (not a gloas block)")
    return body["signed_execution_payload_bid"]["message"]


def classify_bid(message):
    """'builder' or 'self' for one execution payload bid message."""
    raw = message.get("builder_index")
    try:
        index = int(raw)
    except (TypeError, ValueError):
        raise ValueError("bid has no usable builder_index: {0!r}".format(raw)) from None
    return "self" if index == BUILDER_INDEX_SELF_BUILD else "builder"


def is_payment_only(message):
    """True when the bid pays nothing through value and something through execution_payment."""
    return int(message.get("value", "0")) == 0 and int(message.get("execution_payment", "0")) > 0


def classify_window(bn, first, last):
    lists = {"builder": [], "self": [], "payment_only": []}
    errs, missed = [], 0
    for slot in range(first, last + 1):
        try:
            with urllib.request.urlopen("{0}/eth/v2/beacon/blocks/{1}".format(bn, slot), timeout=5) as r:
                message = bid_message(json.load(r))
            kind = classify_bid(message)
            lists[kind].append(slot)
            if kind == "builder" and is_payment_only(message):
                lists["payment_only"].append(slot)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                missed += 1  # no block proposed in this slot
            else:
                errs.append((slot, "HTTP {0} {1}".format(e.code, e.reason)))
        except Exception as e:
            errs.append((slot, "{0}: {1}".format(type(e).__name__, e)))
    if errs:
        w = sys.stderr
        print("epbs-blocks: {0} of {1} slots in [{2},{3}] could NOT be classified "
              "and are counted in NO list; empty-slot 404s: {4}"
              .format(len(errs), last - first + 1, first, last, missed), file=w)
        for slot, why in errs[:10]:
            print("  slot {0}: {1}".format(slot, why), file=w)
        if len(errs) > 10:
            print("  ... and {0} more".format(len(errs) - 10), file=w)
    for name in ("builder", "self", "payment_only"):
        print(name + "=" + ",".join(map(str, lists[name])))


def self_test():
    def bid(index, value="0", execution_payment="0"):
        return {"builder_index": index, "value": value, "execution_payment": execution_payment}

    self_build = str(BUILDER_INDEX_SELF_BUILD)
    cases = [
        ("self-build", bid(self_build), "self", False),
        ("builder paying through value", bid("1", value="100000"), "builder", False),
        ("builder paying only through execution_payment", bid("1", execution_payment="100000"), "builder", True),
        ("builder paying through both", bid("1", value="5", execution_payment="7"), "builder", False),
        ("builder index 0 with a zero bid", bid("0"), "builder", False),
        ("index just below the self-build marker", bid(str(BUILDER_INDEX_SELF_BUILD - 1)), "builder", False),
        ("integer builder_index", bid(BUILDER_INDEX_SELF_BUILD), "self", False),
    ]
    failed = 0
    for name, message, kind, payment_only in cases:
        got = (classify_bid(message), is_payment_only(message))
        if got != (kind, payment_only):
            failed += 1
            print("FAIL {0}: got {1}, want {2}".format(name, got, (kind, payment_only)))
    for name, message in [("missing builder_index", {"value": "0"}),
                          ("non-numeric builder_index", bid("Infinity"))]:
        try:
            classify_bid(message)
            failed += 1
            print("FAIL {0}: classified instead of raising".format(name))
        except ValueError:
            pass
    try:
        bid_message({"data": {"message": {"body": {}}}})
        failed += 1
        print("FAIL a body without a bid: returned instead of raising")
    except ValueError:
        pass
    total = len(cases) + 3
    print("epbs-blocks self-test: {0}/{1} passed".format(total - failed, total))
    return 1 if failed else 0


def main(argv):
    if len(argv) == 2 and argv[1] == "self-test":
        return self_test()
    if len(argv) == 5 and argv[1] == "classify":
        classify_window(argv[2], int(argv[3]), int(argv[4]))
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
