#!/usr/bin/env python3
"""Verify the BLS signature of every SignedExecutionPayloadBid Helix served to CB, from the capture.

CB (21fcef9) does not check the bid signature itself: its validate_bid compares header fields
only, and the beacon node is the verifier. This checks it offline against the relay key, under
DOMAIN_BEACON_BUILDER at the Gloas fork version, for both transports:
  - stream: websocket binary frames from :4040 (2-byte frame header [kind=1, fork=7] + SSZ)
  - http:   200 application/octet-stream responses to POST .../execution_payload_bid/...

hash_tree_root follows lighthouse f064e9e / tree_hash 03d9fa4: ExecutionPayloadBid is a progressive
container (12 active fields) and blob_kzg_commitments a progressive list, with
merkleize_progressive(c, n) = H(merkleize(c[:n], n), merkleize_progressive(c[n:], 4n)).

Known-negatives per bid: one flipped message byte, and the same bid under the genesis fork version,
must both FAIL, so a PASS is not a property of the verifier.

Usage (needs py_ecc):
  uv run --no-project --with py_ecc python3 verify_bids.py <pcap> <gloas_fork_version> \
      <genesis_validators_root> <relay_pubkey> [genesis_fork_version]
"""
import hashlib
import subprocess
import sys
from pathlib import Path

from py_ecc.bls import G2ProofOfPossession as bls

NETSHOOT = "nicolaka/netshoot:latest"
ZERO = b"\x00" * 32
DOMAIN_BEACON_BUILDER = bytes.fromhex("0b000000")


def h(a, b):
    return hashlib.sha256(a + b).digest()


def merkleize(chunks, n):
    layer = list(chunks) + [ZERO] * (n - len(chunks))
    while len(layer) > 1:
        layer = [h(layer[i], layer[i + 1]) for i in range(0, len(layer), 2)]
    return layer[0]


def merkleize_progressive(chunks, n=1):
    if not chunks:
        return ZERO
    return h(merkleize(chunks[:n], n), merkleize_progressive(chunks[n:], n * 4))


def pad32(b):
    return b + b"\x00" * (32 - len(b))


def u64(b):
    return pad32(b)  # SSZ uint64 is already little-endian


def bid_message_root(m):
    """hash_tree_root(ExecutionPayloadBid) from its SSZ bytes."""
    off = int.from_bytes(m[188:192], "little")
    assert off == 224, f"unexpected list offset {off}"
    commits = m[224:]
    assert len(commits) % 48 == 0
    croots = [h(c[:32], pad32(c[32:48])) for c in (commits[i:i + 48] for i in range(0, len(commits), 48))]
    commitments_root = h(merkleize_progressive(croots), (len(croots)).to_bytes(32, "little"))
    fields = [
        m[0:32], m[32:64], m[64:96], m[96:128],  # parent_block_hash, parent_block_root, block_hash, prev_randao
        pad32(m[128:148]),                         # fee_recipient
        u64(m[148:156]), u64(m[156:164]), u64(m[164:172]), u64(m[172:180]), u64(m[180:188]),
        commitments_root,
        m[192:224],                                # execution_requests_root
    ]
    active = bytearray(32)
    for i in range(len(fields)):
        active[i // 8] |= 1 << (i % 8)
    return h(merkleize_progressive(fields), bytes(active))


def domain(fork_version, gvr):
    fork_data_root = h(pad32(fork_version), gvr)
    return DOMAIN_BEACON_BUILDER + fork_data_root[:28]


def verify(ssz, dom, pk):
    off = int.from_bytes(ssz[0:4], "little")
    assert off == 100, f"unexpected message offset {off}"
    sig, msg = ssz[4:100], ssz[100:]
    root = h(bid_message_root(msg), dom)
    return bls.Verify(pk, root, sig), msg


def tshark(pcap, display_filter, fields):
    d = Path(pcap).resolve().parent
    cmd = ["docker", "run", "--rm", "-v", f"{d}:/data:ro", NETSHOOT, "tshark", "-r", f"/data/{Path(pcap).name}",
           "-d", "tcp.port==4040,http", "-Y", display_filter, "-T", "fields", "-E", "separator=\t"]
    for f in fields:
        cmd += ["-e", f]
    out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    return [(line.split("\t") + [""] * len(fields))[:len(fields)] for line in out.splitlines() if line.strip()]


def main():
    pcap, fork_hex, gvr_hex, pk_hex = sys.argv[1:5]
    genesis_fork_hex = sys.argv[5] if len(sys.argv) > 5 else "0x10000038"
    gvr = bytes.fromhex(gvr_hex.removeprefix("0x"))
    pk = bytes.fromhex(pk_hex.removeprefix("0x"))
    dom = domain(bytes.fromhex(fork_hex.removeprefix("0x")), gvr)
    wrong_dom = domain(bytes.fromhex(genesis_fork_hex.removeprefix("0x")), gvr)

    bids = []  # (transport, frame, ssz)
    for frame, payload in tshark(pcap, "websocket.opcode==2 && tcp.srcport==4040", ["frame.number", "websocket.payload"]):
        b = bytes.fromhex(payload.replace(":", ""))
        assert b[:2] == b"\x01\x07", f"frame {frame}: header {b[:2].hex()}"
        bids.append(("stream", frame, b[2:]))
    reqs = {f: (m, u) for f, m, u in tshark(pcap, "http.request", ["frame.number", "http.request.method", "http.request.uri"])}
    http_codes = {}
    for frame, code, ctype, req_in, body in tshark(
            pcap, "http.response", ["frame.number", "http.response.code", "http.content_type", "http.request_in", "http.file_data"]):
        method, uri = reqs.get(req_in, ("?", "?"))
        if "/execution_payload_bid/" not in uri:
            continue
        http_codes[(method, code, ctype)] = http_codes.get((method, code, ctype), 0) + 1
        if code == "200":
            assert ctype == "application/octet-stream", f"frame {frame}: {ctype}"
            bids.append(("http", frame, bytes.fromhex(body.replace(":", ""))))

    print(f"relay pubkey {pk_hex}\nfork_version {fork_hex} domain {dom.hex()}")
    print("http responses to .../execution_payload_bid/ (method, status, content-type): "
          + ", ".join(f"{k}={v}" for k, v in sorted(http_codes.items())))
    tally = {}
    for transport, frame, ssz in bids:
        ok, msg = verify(ssz, dom, pk)
        flipped = bytearray(ssz)
        flipped[100 + 70] ^= 0x01  # inside block_hash
        neg_byte, _ = verify(bytes(flipped), dom, pk)
        neg_dom, _ = verify(ssz, wrong_dom, pk)
        slot = int.from_bytes(msg[164:172], "little")
        builder_index = int.from_bytes(msg[148 + 8:164], "little")
        value = int.from_bytes(msg[172:180], "little")
        payment = int.from_bytes(msg[180:188], "little")
        t = tally.setdefault(transport, {"n": 0, "valid": 0, "neg_fired": 0, "slots": set(), "builder_index": set()})
        t["n"] += 1
        t["valid"] += ok
        t["neg_fired"] += (not neg_byte) and (not neg_dom)
        t["slots"].add(slot)
        t["builder_index"].add(builder_index)
        print(f"{transport:6} frame={frame:>6} slot={slot:<4} builder_index={builder_index} value_gwei={value} "
              f"execution_payment_gwei={payment} sig={'VALID' if ok else 'INVALID'} "
              f"negatives={'fail-as-expected' if (not neg_byte and not neg_dom) else 'UNEXPECTED-PASS'}")
    print()
    for transport, t in sorted(tally.items()):
        print(f"{transport}: {t['valid']}/{t['n']} signatures valid over slots {min(t['slots'])}..{max(t['slots'])}; "
              f"known-negatives rejected on {t['neg_fired']}/{t['n']}; builder_index {sorted(t['builder_index'])}")
    ok = all(t["valid"] == t["n"] and t["neg_fired"] == t["n"] for t in tally.values()) and tally
    print("RESULT", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
