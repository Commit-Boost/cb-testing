#!/usr/bin/env python3
"""Per-slot and per-delay table for a stream_tail.sh run.

For every bid slot: the downlink delay in force, what Helix sent on the bid stream
(accept line, each `streamed bid`, `bid stream closed delivered=N`), what CB did with it
(X-Timeout-Ms it sent, the stream's update count and bid, the raced HTTP bid, the auction
winner and its transport), and what the capture in CB's namespace shows (binary frames
that reached CB before CB's FIN, frames that arrived after it, per-frame one-way delay
from Helix's `streamed bid` log line to the frame's arrival at CB).

The X-Timeout-Ms and Date-Milliseconds CB put on the wire come from cb-helix-upgrades.tsv
when the run wrote one; CB's own budget (`max_timeout_ms`) is shown beside it, since a CB
that subtracts a downlink trip sends less than its budget. With symmetric delay (ips.txt
`uplink=1`) CB's FIN reaches the capture D after CB sent it, so `wire<FIN` overcounts;
`wire<end` counts frames that arrived before CB's own end-of-read log line instead.

Usage: analyze_tail.py <out-dir>
"""
import json
import re
import statistics
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
TS = re.compile(r"^\[[\w-]+\] (\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z")


def ts(line):
    m = TS.match(line)
    if not m:
        return None
    whole, frac = m.group(1).split(".")
    t = datetime.strptime(whole, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc).timestamp()
    return t + float("0." + frac)


def eth(wei_or_gwei, unit):
    return float(wei_or_gwei) / (1e18 if unit == "wei" else 1e9)


def load_delays(d):
    out = {}
    for row in (d / "delays.tsv").read_text().split("\n"):
        if row.strip():
            delay, first, last = (int(x) for x in row.split("\t"))
            for s in range(first, last + 1):
                out.setdefault(s, delay)  # control stretch comes last; never overlaps
    return out


def helix_streams(path):
    """Gloas bid streams, in log order. `streamed bid` / `bid stream closed` carry no slot,
    so each attaches to the most recent accept."""
    accept_re = re.compile(
        # Helix 2a8f7d5e logs "accepting execution payload bid stream", 7d3fc659 "accepting bid stream"
        r"accepting (?:execution payload )?bid stream ms_into_slot=(\d+) .*?timeout_ms=(\d+) handshake_ms=(\d+) "
        r".*?start_in_ms=(\d+) window_ms=(\d+) .*?slot=(\d+)")
    sent_re = re.compile(r"execution_payload_bid_stream: streamed bid block_hash=(0x[0-9a-f]+) execution_payment=(\d+)")
    closed_re = re.compile(r"execution_payload_bid_stream: bid stream closed delivered=(\d+) hit_deadline=(\w+)")
    streams, cur = {}, None
    for raw in path.read_text(errors="replace").splitlines():
        line = ANSI.sub("", raw)
        if m := accept_re.search(line):
            ms, to, hs, start_in, win, slot = (int(x) for x in m.groups())
            cur = streams[slot] = {"t_accept": ts(line), "ms_into_slot": ms, "timeout_ms": to,
                                   "handshake_ms": hs, "start_in_ms": start_in, "window_ms": win,
                                   "sent": [], "delivered": None, "hit_deadline": None, "t_closed": None}
        elif cur is not None and (m := sent_re.search(line)):
            cur["sent"].append((ts(line), m.group(1), eth(m.group(2), "gwei")))
        elif cur is not None and (m := closed_re.search(line)):
            cur["delivered"], cur["hit_deadline"], cur["t_closed"] = int(m.group(1)), m.group(2), ts(line)
            cur = None
    return streams


def cb_requests(path):
    rid_re = re.compile(r"req_id=([0-9a-f-]{36})")
    slot_re = re.compile(r"(?<!\w)slot=(\d+)")
    reqs = {}
    for raw in path.read_text(errors="replace").splitlines():
        line = ANSI.sub("", raw)
        if "execution_payload_bid/{slot}" not in line:
            continue
        rid, slot = rid_re.search(line), slot_re.search(line)
        if rid and slot:
            reqs.setdefault(rid.group(1), {"slot": int(slot.group(1)), "lines": []})["lines"].append(line)
    by_slot = {}
    for r in reqs.values():
        text = "\n".join(r["lines"])
        g = lambda pat: (m.group(1) if (m := re.search(pat, text)) else None)
        ws = re.search(r'received new bid from ws stream .*?execution_payment_eth="([\d.]+)".*?updates=(\d+)', text)
        end_line = next((l for l in r["lines"] if "received new bid from ws stream" in l or re.search(r" no bid relay_id=", l)), None)
        if ws:
            stream_status, stream_bid, updates = "200", float(ws.group(1)), int(ws.group(2))
        elif re.search(r" no bid relay_id=", text):
            stream_status, stream_bid, updates = "204", None, 0
        elif "stream failed" in text:
            stream_status, stream_bid, updates = "error", None, 0
        else:
            stream_status, stream_bid, updates = "?", None, None
        win = re.search(r'auction winner .*?transport="(\w+)" bid_eth="([\d.]+)"', text)
        http = g(r'received new header .*?bid_eth="([\d.]+)"')
        by_slot[r["slot"]] = {
            "x_timeout_ms": int(g(r"max_timeout_ms=(\d+)") or -1),
            "connect_latency": g(r"ws connected .*?connect_latency=([\d.]+m?s)"),
            "http_bid": float(http) if http else None,
            "http_latency": g(r"received new header .*?latency=([\d.]+m?s)"),
            "stream_status": stream_status, "stream_bid": stream_bid, "updates": updates,
            "winner": win.group(1) if win else None, "served": float(win.group(2)) if win else None,
            "stream_failed": text.count("stream failed"),
            "responded": g(r"Responded with (\d+ \w+ in \d+ ms)"),
            "t_stream_end": ts(end_line) if end_line else None,
        }
    return by_slot


def upgrades(path):
    """Per slot: the X-Timeout-Ms and Date-Milliseconds of the stream upgrade CB sent."""
    out = {}
    if not path.exists():
        return out
    for row in path.read_text().splitlines():
        f = (row.split("\t") + [""] * 4)[:4]
        m = re.search(r"execution_payload_bid_stream/(\d+)/", f[2])
        if not m:
            continue
        hdrs = {}
        for line in f[3].split("|"):
            k, _, v = line.replace("\\r\\n", "").partition(":")
            hdrs[k.strip().lower()] = v.strip()
        out[int(m.group(1))] = {"xto": int(hdrs["x-timeout-ms"]) if "x-timeout-ms" in hdrs else None,
                                "date_ms": int(hdrs["date-milliseconds"]) if "date-milliseconds" in hdrs else None}
    return out


def capture(path):
    """Per websocket connection: slot, relay->CB binary-frame arrivals, close, CB FIN, RSTs."""
    conns = defaultdict(lambda: {"slot": None, "frames": [], "close": None, "cb_fin": None, "rst": set(),
                                 "t_get": None, "t_101": None})
    for row in path.read_text().splitlines():
        f = (row.split("\t") + [""] * 9)[:9]
        t, stream, sport, fin, rst, opcodes, uri, code, _tlen = f
        if not stream:
            continue
        c, t = conns[stream], float(t)
        relay = sport == "4040"
        if m := re.search(r"execution_payload_bid_stream/(\d+)/", uri):
            c["slot"], c["t_get"] = int(m.group(1)), t
        if code == "101" and relay:
            c["t_101"] = t
        ops = [o for o in opcodes.split(",") if o]
        if relay:
            c["frames"] += [t] * ops.count("2")
            if "8" in ops and c["close"] is None:
                c["close"] = t
        if fin in ("1", "True") and not relay and c["cb_fin"] is None:
            c["cb_fin"] = t
        if rst in ("1", "True"):
            c["rst"].add("relay" if relay else "cb")
    return {c["slot"]: c for c in conns.values() if c["slot"] is not None}


def fmt(v, spec=".2f"):
    return "-" if v is None else format(v, spec)


def main():
    d = Path(sys.argv[1])
    delays = load_delays(d)
    hx = helix_streams(d / "helix-relay.log")
    cb = cb_requests(d / "cb-epbs.log")
    cap = capture(d / "cb-helix.tsv")
    up = upgrades(d / "cb-helix-upgrades.tsv")
    ips = (d / "ips.txt").read_text() if (d / "ips.txt").exists() else ""
    uplink = "uplink=1" in ips
    genesis = int(json.loads((d / "genesis.json").read_text())["data"]["genesis_time"])
    sps = int(json.loads((d / "spec.json").read_text())["data"]["SECONDS_PER_SLOT"])
    print(f"# delay: {'SYMMETRIC (D each way, connect RTT 2D)' if uplink else 'downlink only (relay -> CB)'}")

    rows = []
    for slot in sorted(delays):
        h, c, p = hx.get(slot), cb.get(slot), cap.get(slot)
        if not h or not c:
            rows.append({"slot": slot, "D": delays[slot], "missing": "helix" if not h else "cb"})
            continue
        fin = p["cb_fin"] if p else None
        w = up.get(slot, {})
        end = c["t_stream_end"]
        slot_ms = lambda t: None if t is None else t * 1000 - (genesis + slot * sps) * 1000
        on_wire = len(p["frames"]) if p else None
        before_fin = sum(1 for t in p["frames"] if fin is None or t < fin) if p else None
        owd = [(a - s[0]) * 1000 for a, s in zip(p["frames"], h["sent"])] if p else []
        rows.append({
            "slot": slot, "D": delays[slot],
            "x_timeout": c["x_timeout_ms"], "xto_wire": w.get("xto"), "hx_timeout": h["timeout_ms"],
            "handshake": h["handshake_ms"],
            # Helix's window end (Date-Milliseconds + X-Timeout-Ms) and CB's end of read, ms into the slot
            "hx_end_ms": (None if w.get("xto") is None or w.get("date_ms") is None else
                          slot_ms((w["date_ms"] + w["xto"]) / 1000)),
            "cb_end_ms": slot_ms(end),
            "before_end": sum(1 for t in p["frames"] if end is not None and t < end) if p else None,
            "start_in": h["start_in_ms"], "window": h["window_ms"], "ms_into_slot": h["ms_into_slot"],
            "sent": h["delivered"], "sent_logged": len(h["sent"]), "hit_deadline": h["hit_deadline"],
            "last_sent": h["sent"][-1][2] if h["sent"] else None,
            "recv": c["updates"], "on_wire": on_wire, "before_fin": before_fin,
            "stream_bid": c["stream_bid"], "http_bid": c["http_bid"], "winner": c["winner"], "served": c["served"],
            "status": c["stream_status"], "connect": c["connect_latency"],
            "owd_med": statistics.median(owd) if owd else None,
            "owd_max": max(owd) if owd else None,
            "close_vs_fin": (None if not p or p["close"] is None or fin is None else (p["close"] - fin) * 1000),
            "rst": ",".join(sorted(p["rst"])) if p and p["rst"] else "-",
            # Helix's own end vs CB's give-up, on the host clock (CB FIN from the capture)
            # the trace's model: a frame lands iff Helix sent it more than D before CB stops reading
            "pred_recv": (None if c["t_stream_end"] is None else
                          sum(1 for t, _, _ in h["sent"] if t + delays[slot] / 1000 < c["t_stream_end"])),
            # Helix's close (its deadline) minus CB's end of read (its deadline), host clock
            "hx_end_minus_cb_end": (None if c["t_stream_end"] is None or h["t_closed"] is None else
                                    (h["t_closed"] - c["t_stream_end"]) * 1000),
        })

    hdr = ("D_ms slot cb_budget xto_wire hx_timeout handshake start_in window hxEnd@ms cbEnd@ms | sent recv "
           "wire<end wire<FIN wire | last_sent cb_stream http served winner status | owd_med owd_max close-FIN_ms "
           "rst | pred_recv hxEnd-cbEnd_ms")
    print(hdr)
    for r in rows:
        if "missing" in r:
            print(f"{r['D']:>4} {r['slot']:>4}  (no {r['missing']} record)")
            continue
        print(f"{r['D']:>4} {r['slot']:>4} {r['x_timeout']:>9} {r['xto_wire']!s:>8} {r['hx_timeout']:>10} "
              f"{r['handshake']:>9} {r['start_in']:>8} {r['window']:>6} {fmt(r['hx_end_ms'], '.0f'):>8} "
              f"{fmt(r['cb_end_ms'], '.0f'):>8} | {r['sent']:>4} {r['recv']!s:>4} {r['before_end']!s:>8} "
              f"{r['before_fin']!s:>8} "
              f"{r['on_wire']!s:>4} | {fmt(r['last_sent']):>9} {fmt(r['stream_bid']):>9} {fmt(r['http_bid']):>4} "
              f"{fmt(r['served']):>6} {r['winner']!s:>6} {r['status']:>6} | {fmt(r['owd_med'], '.1f'):>7} "
              f"{fmt(r['owd_max'], '.1f'):>7} {fmt(r['close_vs_fin'], '.1f'):>12} {r['rst']:>3} | {r['pred_recv']!s:>9} "
              f"{fmt(r['hx_end_minus_cb_end'], '.1f'):>14}")

    print("\nper delay (control = the final D=0 stretch):")
    print("D_ms   slots  sent(mean) recv(mean) recv/sent  served==last_sent  stream_won  http_won  "
          "stream_status  owd_med(ms)  handshake(mean)  start_in(mean)  recv==pred  hxEnd-cbEnd(median ms)  "
          "xto_wire(mean)  cb_budget(mean)  served(mean)")
    groups = defaultdict(list)
    seen_ranges = [tuple(int(x) for x in row.split("\t")) for row in (d / "delays.tsv").read_text().split("\n") if row.strip()]
    for i, (delay, first, last) in enumerate(seen_ranges):
        label = f"{delay}" + (" (control)" if i == len(seen_ranges) - 1 and i > 0 and delay == 0 else "")
        groups[label] = [r for r in rows if first <= r["slot"] <= last and "missing" not in r]
    for label, rs in groups.items():
        if not rs:
            continue
        mean = lambda k: statistics.mean(r[k] for r in rs if r[k] is not None)
        sent, recv = mean("sent"), mean("recv")
        same = sum(1 for r in rs if r["served"] is not None and r["last_sent"] is not None and abs(r["served"] - r["last_sent"]) < 1e-9)
        sw = sum(1 for r in rs if r["winner"] == "stream")
        hw = sum(1 for r in rs if r["winner"] == "http")
        st = defaultdict(int)
        for r in rs:
            st[r["status"]] += 1
        owds = [r["owd_med"] for r in rs if r["owd_med"] is not None]
        print(f"{label:<14} {len(rs):>3} {sent:>10.2f} {recv:>10.2f} {recv / sent if sent else 0:>9.2f} "
              f"{same:>10}/{len(rs):<6} {sw:>10} {hw:>9}  {dict(st)!s:<14} "
              f"{fmt(statistics.median(owds) if owds else None, '.1f'):>10} {mean('handshake'):>15.1f} {mean('start_in'):>15.1f} "
              f"{sum(1 for r in rs if r['recv'] == r['pred_recv']):>8}/{len(rs):<3} "
              f"{fmt(statistics.median(r['hx_end_minus_cb_end'] for r in rs if r['hx_end_minus_cb_end'] is not None), '.2f'):>10} "
              f"{fmt(statistics.mean(r['xto_wire'] for r in rs if r['xto_wire'] is not None) if any(r['xto_wire'] is not None for r in rs) else None, '.1f'):>15} "
              f"{mean('x_timeout'):>15.1f} {fmt(statistics.mean(r['served'] for r in rs if r['served'] is not None) if any(r['served'] is not None for r in rs) else None, '.3f'):>12}")


if __name__ == "__main__":
    main()
