#!/usr/bin/env python3
"""Time a TCP echo server in a guest, from the host, through QEMU's own
network.

A card plugged into QEMU's user network with a port forwarded
(`UNET_NET=user,net=10.0.9.0/24,host=10.0.9.1,hostfwd=tcp:127.0.0.1:5800-10.0.9.15:7000`
in zCore/Makefile) makes a connection to 127.0.0.1:5800 on the host a
connection from 10.0.9.1 to port 7000 of the guest. The other end of every
connection is then a TCP that is not ours: QEMU's.

It is how the network stack that runs as a process (tools/eclipse-netstack)
is measured against the kernel's: the same card, the same driver, the same
`eclipse-tcp-echo 7000` in the guest, and whichever stack has the address
takes the connections.

    scripts/tcp_bench.py 5800 -c 2000            # one connection, echoes one at a time
    scripts/tcp_bench.py 5800 -c 2000 -s 1400
    scripts/tcp_bench.py 5800 --bulk 8           # 8 MB there and back at once
    scripts/tcp_bench.py 5800 --connections 300  # connect, one echo, close

Exit status: 0 when everything came back as it was sent, 1 when it did not
or a connection failed.
"""

import argparse
import socket
import statistics
import sys
import threading
import time


def connect(args) -> socket.socket:
    s = socket.create_connection((args.host, args.port), timeout=args.patience)
    # One segment for each write, when it is written.
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return s


def read_exactly(s: socket.socket, n: int) -> bytes:
    got = bytearray()
    while len(got) < n:
        more = s.recv(n - len(got))
        if not more:
            break
        got += more
    return bytes(got)


def report(what: str, trips: list, took: float) -> None:
    us = sorted(t / 1000 for t in trips)
    nth = lambda p: us[min(len(us) - 1, int(len(us) * p))]
    print(
        f"round trip, us: min {us[0]:.0f}  median {statistics.median(us):.0f}  mean {statistics.fmean(us):.0f}"
        f"  p90 {nth(0.90):.0f}  p99 {nth(0.99):.0f}  max {us[-1]:.0f}"
    )
    print(f"{what} a second: {len(trips) / (took / 1e9):.0f}")


def echoes(args) -> int:
    """One connection, `count` messages of `size` bytes, each sent when the
    one before came back."""
    data = bytes(i & 0xFF for i in range(args.size))
    s = connect(args)
    trips, bad = [], 0
    start = time.monotonic_ns()
    for _ in range(args.count):
        t = time.monotonic_ns()
        s.sendall(data)
        back = read_exactly(s, len(data))
        if back != data:
            bad += 1
            break
        trips.append(time.monotonic_ns() - t)
    took = time.monotonic_ns() - start
    s.close()
    print(f"{args.count} echoes of {args.size} bytes on one connection: {len(trips)} answered, {args.count - len(trips)} lost, {took / 1e9:.3f} s")
    if trips:
        report("echoes", trips, took)
    return 1 if bad or len(trips) < args.count else 0


def bulk(args) -> int:
    """`bulk` megabytes sent while they are read back: both directions of
    one connection full at once."""
    total = args.bulk * 1024 * 1024
    piece = bytes(i * 7 & 0xFF for i in range(64 * 1024))
    s = connect(args)
    s.settimeout(args.patience)
    failed = []

    def send():
        try:
            for _ in range(total // len(piece)):
                s.sendall(piece)
            s.shutdown(socket.SHUT_WR)
        except OSError as e:
            failed.append(e)

    start = time.monotonic_ns()
    sender = threading.Thread(target=send)
    sender.start()
    got, good = 0, True
    try:
        while True:
            more = s.recv(256 * 1024)
            if not more:
                break
            at = got % len(piece)
            # What comes back is what was sent, at the place it was sent.
            if more != (piece[at:] + piece * (len(more) // len(piece) + 1))[: len(more)]:
                good = False
            got += len(more)
    except OSError as e:
        failed.append(e)
    sender.join()
    took = time.monotonic_ns() - start
    s.close()
    for e in failed:
        print(f"tcp_bench: {e}")
    print(f"{args.bulk} MB there and back: {got} of {total} bytes back, {'intact' if good else 'DAMAGED'}, {took / 1e9:.3f} s")
    print(f"megabytes a second, each way: {got / (1024 * 1024) / (took / 1e9):.2f}")
    return 0 if good and got == total and not failed else 1


def connections(args) -> int:
    """`connections` times: connect, one echo of `size` bytes, close."""
    data = bytes(i & 0xFF for i in range(args.size))
    trips, bad = [], 0
    start = time.monotonic_ns()
    for _ in range(args.connections):
        t = time.monotonic_ns()
        try:
            s = connect(args)
            s.sendall(data)
            back = read_exactly(s, len(data))
            s.close()
        except OSError as e:
            print(f"tcp_bench: {e}")
            bad += 1
            continue
        if back != data:
            bad += 1
            continue
        trips.append(time.monotonic_ns() - t)
    took = time.monotonic_ns() - start
    print(f"{args.connections} connections with an echo of {args.size} bytes each: {len(trips)} answered, {bad} lost, {took / 1e9:.3f} s")
    if trips:
        report("connections", trips, took)
    return 1 if bad else 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("port", type=int, help="the port on the host that is forwarded to the guest")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("-c", "--count", type=int, default=1000, help="echoes on one connection")
    p.add_argument("-s", "--size", type=int, default=56, help="bytes of each echo")
    p.add_argument("--bulk", type=int, metavar="MB", help="megabytes to send and read back at once")
    p.add_argument("--connections", type=int, metavar="N", help="connections, one echo each")
    p.add_argument("--patience", type=float, default=30.0, help="seconds to wait for anything")
    args = p.parse_args()
    try:
        if args.bulk is not None:
            return bulk(args)
        if args.connections is not None:
            return connections(args)
        return echoes(args)
    except OSError as e:
        print(f"tcp_bench: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
