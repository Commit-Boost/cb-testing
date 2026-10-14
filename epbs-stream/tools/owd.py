#!/usr/bin/env python3
"""One-way delay probe between two network namespaces on the same host.

Both ends read the same kernel clock, so receive time minus the send timestamp
carried in the datagram is the one-way delay in that direction.

  owd.py recv <port> <count> [timeout_s]   prints the per-probe one-way delays (ms)
  owd.py send <host> <port> <count>        sends <count> probes 50 ms apart
"""
import socket
import struct
import sys
import time


def recv(port, count, timeout_s):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("0.0.0.0", port))
    s.settimeout(timeout_s)
    delays = []
    try:
        while len(delays) < count:
            data, _ = s.recvfrom(64)
            now = time.time()
            (sent,) = struct.unpack("!d", data[:8])
            delays.append((now - sent) * 1000)
    except socket.timeout:
        pass
    if delays:
        delays.sort()
        print(f"n={len(delays)} min={delays[0]:.1f} median={delays[len(delays) // 2]:.1f} max={delays[-1]:.1f} ms")
    else:
        print("n=0")


def send(host, port, count):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for _ in range(count):
        s.sendto(struct.pack("!d", time.time()), (host, port))
        time.sleep(0.05)


if __name__ == "__main__":
    if sys.argv[1] == "recv":
        recv(int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4]) if len(sys.argv) > 4 else 8.0)
    else:
        send(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))
