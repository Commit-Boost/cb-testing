#!/usr/bin/env python3
"""Score a scenario directory written by scenario.sh against the pass criteria.

  a. no-bid slots: CB logs no "Connection reset without closing handshake" / "stream failed",
     and cb_pbs_relay_stream_fallback_total{endpoint="get_execution_payload_bid_stream"} does
     not move between the pause and resume snapshots
  b. bid slots: CB still picks stream-carried bids (`auction winner ... transport="stream"`, or
     since #509 "received new bid from ws stream")
  c. the stream ends with a close frame: in the capture, the relay sends a WebSocket close on
     every no-bid stream, and CB logs a clean end ("no bid") for each no-bid request
  negative: negative.sh refused all seven bad upgrades with a 4xx

Usage: analyze.py <scenario-dir>
"""
import re
import sys
from collections import defaultdict
from pathlib import Path

STREAM = "get_execution_payload_bid_stream"
RESET = "Connection reset without closing handshake"


def load_slots(d):
    env = dict(line.split("=", 1) for line in (d / "slots.env").read_text().split())
    s0, pause, resume, end = (int(env[k]) for k in ("S0", "PAUSE", "RESUME", "END"))
    bids = list(range(s0 + 1, pause + 1)) + list(range(resume + 1, end + 1))
    return bids, list(range(pause + 1, resume + 1))


def cb_requests(path):
    """Per get-bid request (req_id): its slot and the log lines that matter."""
    rid_re = re.compile(r"req_id=([0-9a-f-]{36})")
    slot_re = re.compile(r"(?<!\w)slot=(\d+)")
    reqs = {}
    for line in path.read_text(errors="replace").splitlines():
        if "execution_payload_bid/{slot}" not in line:
            continue
        rid, slot = rid_re.search(line), slot_re.search(line)
        if not rid or not slot:
            continue
        r = reqs.setdefault(rid.group(1), {"slot": int(slot.group(1)), "lines": []})
        r["lines"].append(line)
    by_slot = defaultdict(list)
    for r in reqs.values():
        text = "\n".join(r["lines"])
        # "auction winner" before the ePBS route logged its own "selected bid" line. Since
        # #509 dropped the HTTP race, a stream relay's ePBS bid comes only over the stream
        # and CB logs no transport field, so the stream's own receipt line is the winner.
        winner = re.search(r'(?:auction winner|selected bid) .*?transport="(\w+)"', text)
        winner = winner.group(1) if winner else ("stream" if "received new bid from ws stream" in text else None)
        by_slot[r["slot"]].append({
            "winner": winner,
            "no_header": "no header available for slot" in text or "no bid for slot" in text,
            "resets": text.count(RESET),
            "stream_failed": text.count("stream failed"),
            "clean_no_bid": len(re.findall(r" no bid relay_id=", text)),
            "stream_bid": "received new bid from ws stream" in text,
        })
    return by_slot


def metric(path, name, **labels):
    """Sum of the matching series, or None when none exists (a labelled counter only
    appears after its first increment)."""
    total = None
    for line in path.read_text().splitlines():
        if not line.startswith(name + "{"):
            continue
        if all(f'{k}="{v}"' in line for k, v in labels.items()):
            total = (total or 0.0) + float(line.rsplit(" ", 1)[1])
    return total


def show(values):
    return "/".join("absent" if v is None else f"{v:g}" for v in values)


def delta(values):
    return (values[1] or 0.0) - (values[0] or 0.0)


def capture(path):
    """Per websocket stream connection (bid stream URI): slot and close/FIN/RST timeline."""
    streams = defaultdict(lambda: {"slot": None, "status": None, "events": []})
    for row in path.read_text().splitlines():
        f = (row.split("\t") + [""] * 8)[:8]
        t, stream, sport, fin, rst, opcodes, uri, code = f
        if not stream:
            continue
        s = streams[stream]
        side = "relay" if sport == "4040" else "cb"
        m = re.search(r"execution_payload_bid_stream/(\d+)/", uri)
        if m:
            s["slot"] = int(m.group(1))
        if code:
            s["status"] = code
        if "8" in opcodes.split(","):
            s["events"].append((float(t), f"close:{side}"))
        if fin in ("1", "True"):
            s["events"].append((float(t), f"fin:{side}"))
        if rst in ("1", "True"):
            s["events"].append((float(t), f"rst:{side}"))
    by_slot = defaultdict(list)
    for s in streams.values():
        if s["slot"] is not None:
            by_slot[s["slot"]].append(s)
    return by_slot


def first(events, kind):
    return next((t for t, e in sorted(events) if e == kind), None)


def main():
    d = Path(sys.argv[1])
    bid_slots, nobid_slots = load_slots(d)
    reqs = cb_requests(d / "cb-epbs.log")
    caps = capture(d / "cb-helix.tsv")
    ok = {}

    print(f"bid slots {bid_slots[0]}..{bid_slots[-1]} (minus the no-bid stretch), "
          f"no-bid stretch {nobid_slots[0]}..{nobid_slots[-1]} ({len(nobid_slots)} slots)\n")
    print("slot  kind    cb outcome     resets failed clean_end | capture: status relay_close cb_fin rst")
    for slot in sorted(set(bid_slots) | set(nobid_slots)):
        kind = "no-bid" if slot in nobid_slots else "bid"
        rs = reqs.get(slot, [])
        outcome = ",".join((r["winner"] and f"win:{r['winner']}") or ("204" if r["no_header"] else "?") for r in rs) or "no request"
        resets = sum(r["resets"] for r in rs)
        failed = sum(r["stream_failed"] for r in rs)
        clean = sum(r["clean_no_bid"] for r in rs)
        cap = []
        for s in caps.get(slot, []):
            close = first(s["events"], "close:relay")
            cb_fin = first(s["events"], "fin:cb")
            rst = ",".join(sorted({e.split(":")[1] for _, e in s["events"] if e.startswith("rst")})) or "-"
            when = "-" if close is None else ("before cb FIN" if cb_fin is None or close < cb_fin else "after cb FIN")
            cap.append(f"{s['status']} {when} {'yes' if cb_fin else 'no'} {rst}")
        print(f"{slot:<5} {kind:<7} {outcome:<14} {resets:>6} {failed:>6} {clean:>9} | {'; '.join(cap) or 'no stream'}")

    # a. no-bid slots: no resets / stream failures, fallback counter flat
    nb = [r for s in nobid_slots for r in reqs.get(s, [])]
    served_204 = sum(1 for s in nobid_slots if reqs.get(s) and all(r["no_header"] for r in reqs[s]))
    resets = sum(r["resets"] for r in nb)
    failed = sum(r["stream_failed"] for r in nb)
    fb = [metric(d / f"cb-metrics-{p}.txt", "cb_pbs_relay_stream_fallback_total", endpoint=STREAM) for p in ("pause", "resume")]
    s556 = [metric(d / f"cb-metrics-{p}.txt", "cb_pbs_relay_status_code_total", endpoint=STREAM, http_status_code="556") for p in ("pause", "resume")]
    s204 = [metric(d / f"cb-metrics-{p}.txt", "cb_pbs_relay_status_code_total", endpoint=STREAM, http_status_code="204") for p in ("pause", "resume")]
    all_resets = sum(r["resets"] for rs in reqs.values() for r in rs)
    all_failed = sum(r["stream_failed"] for rs in reqs.values() for r in rs)
    ok["a"] = served_204 >= 10 and served_204 == len(nobid_slots) and resets == 0 and failed == 0 and delta(fb) == 0 and delta(s556) == 0
    print(f"\na. {served_204}/{len(nobid_slots)} no-bid slots answered 204 by CB; in them: {resets} '{RESET}', {failed} 'stream failed'")
    print(f"   pause/resume snapshots ('absent' = never incremented since CB started):")
    print(f"   cb_pbs_relay_stream_fallback_total{{endpoint=\"{STREAM}\"}} {show(fb)} (delta {delta(fb):g}); "
          f"status 556 {show(s556)} (delta {delta(s556):g}); status 204 {show(s204)} (delta {delta(s204):g})")
    print(f"   whole run: {all_resets} resets, {all_failed} 'stream failed'  -> {'PASS' if ok['a'] else 'FAIL'}")

    # b. bid slots: stream-carried bids still win
    winners = [r["winner"] for s in bid_slots for r in reqs.get(s, []) if r["winner"]]
    ok["b"] = winners.count("stream") > 0
    print(f"\nb. bid slots with a winner: {len(winners)}; transport=stream {winners.count('stream')}, "
          f"http {winners.count('http')}  -> {'PASS' if ok['b'] else 'FAIL'}")

    # c. close frames on the wire, clean end in CB's log
    nb_streams = [s for slot in nobid_slots for s in caps.get(slot, [])]
    with_close = [s for s in nb_streams if first(s["events"], "close:relay") is not None]
    seen = [s for s in with_close if (lambda c, f: f is None or c < f)(first(s["events"], "close:relay"), first(s["events"], "fin:cb"))]
    clean = sum(1 for r in nb if r["clean_no_bid"])
    all_streams = [s for ss in caps.values() for s in ss]
    all_close = sum(1 for s in all_streams if first(s["events"], "close:relay") is not None)
    ok["c"] = bool(nb_streams) and len(with_close) == len(nb_streams) and len(seen) > 0 and clean == len(nb)
    print(f"\nc. no-bid streams in capture: {len(nb_streams)}; relay sent a close frame on {len(with_close)}, "
          f"{len(seen)} of them before CB's FIN (CB read the close)")
    print(f"   CB debug 'no bid' (stream ended without error, no bid) on {clean}/{len(nb)} no-bid requests")
    print(f"   whole capture: {len(all_streams)} bid streams, {all_close} ended by a relay close frame  -> {'PASS' if ok['c'] else 'FAIL'}")

    neg = (d / "negative.out").read_text() if (d / "negative.out").exists() else ""
    ok["negative"] = "PASS: 7/7" in neg
    print(f"\nnegative.sh: {(neg.strip().splitlines() or ['missing'])[-1]}")

    print("\nRESULT " + " ".join(f"{k}={'PASS' if v else 'FAIL'}" for k, v in ok.items()))
    return 0 if all(ok.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
