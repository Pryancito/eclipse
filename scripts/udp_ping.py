#!/usr/bin/env python3
"""Ping a guest through a QEMU network card whose frames are UDP datagrams,
with ICMP echo or with UDP datagrams that a program in the guest echoes.

A card plugged into `-netdev socket,udp=127.0.0.1:5700,localaddr=127.0.0.1:5701`
(`UNET_NET=...` in zCore/Makefile) sends every frame as a datagram to port
5700 and takes every datagram that arrives on 5701 as a frame. This program
is the only other machine on that wire: it asks who has the guest's address
(ARP), then sends ICMP echo requests and times the answers. With `--udp PORT`
it sends UDP datagrams to that port instead, which times a program's socket
as well as the stack: `eclipse-udp-echo PORT` in the guest sends them back.

It is how the network stack that runs as a process (tools/eclipse-netstack)
is measured against the kernel's: the same card, the same driver, the same
pings, and whichever stack has the address answers them.

    scripts/udp_ping.py 10.0.9.15 -c 2000          # one at a time: round trips
    scripts/udp_ping.py 10.0.9.15 -c 20000 -w 16   # sixteen in flight: pings a second
    scripts/udp_ping.py 10.0.9.15 --udp 7000       # through a program's UDP socket

Exit status: 0 when every ping was answered, 1 when some were lost, 2 when
nobody has the address.
"""

import argparse
import socket
import statistics
import struct
import sys
import time

OUR_MAC = bytes.fromhex("020000000001")
BROADCAST = b"\xff" * 6
ETH_ARP = 0x0806
ETH_IPV4 = 0x0800


def checksum(data: bytes) -> int:
    """The Internet checksum (RFC 1071) of `data`."""
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return ~total & 0xFFFF


def pad(frame: bytes) -> bytes:
    """`frame`, no shorter than Ethernet's shortest (without the checksum)."""
    return frame.ljust(60, b"\0")


def arp(op: int, to_mac: bytes, target_mac: bytes, our_ip: bytes, target_ip: bytes) -> bytes:
    """An ARP packet from us, in its Ethernet frame."""
    packet = struct.pack("!HHBBH", 1, ETH_IPV4, 6, 4, op) + OUR_MAC + our_ip + target_mac + target_ip
    return pad(to_mac + OUR_MAC + struct.pack("!H", ETH_ARP) + packet)


def echo_request(to_mac: bytes, our_ip: bytes, to_ip: bytes, ident: int, seq: int, data: bytes) -> bytes:
    """An ICMP echo request from us, in its IP packet and Ethernet frame."""
    icmp = struct.pack("!BBHHH", 8, 0, 0, ident, seq) + data
    icmp = icmp[:2] + struct.pack("!H", checksum(icmp)) + icmp[4:]
    ip = struct.pack("!BBHHHBBH", 0x45, 0, 20 + len(icmp), seq, 0x4000, 64, 1, 0) + our_ip + to_ip
    ip = ip[:10] + struct.pack("!H", checksum(ip)) + ip[12:]
    return pad(to_mac + OUR_MAC + struct.pack("!H", ETH_IPV4) + ip + icmp)


def echo_reply(frame: bytes, our_ip: bytes, from_ip: bytes, ident: int):
    """The `(seq, data)` of the echo reply `frame` is, or None when it is not
    one for us with this identifier and a checksum that holds."""
    if len(frame) < 42 or frame[12:14] != struct.pack("!H", ETH_IPV4):
        return None
    ip = frame[14:]
    header = (ip[0] & 0xF) * 4
    total = struct.unpack("!H", ip[2:4])[0]
    if ip[0] >> 4 != 4 or ip[9] != 1 or ip[12:16] != from_ip or ip[16:20] != our_ip:
        return None
    if total > len(ip) or checksum(ip[:header]) != 0:
        return None
    icmp = ip[header:total]
    if len(icmp) < 8 or icmp[0] != 0 or checksum(icmp) != 0:
        return None
    got_ident, seq = struct.unpack("!HH", icmp[4:8])
    if got_ident != ident:
        return None
    return seq, icmp[8:]


OUR_PORT = 40000


def udp_datagram(to_mac: bytes, our_ip: bytes, to_ip: bytes, port: int, seq: int, data: bytes) -> bytes:
    """A UDP datagram from us to `port`, in its IP packet and Ethernet frame."""
    length = 8 + len(data)
    udp = struct.pack("!HHHH", OUR_PORT, port, length, 0) + data
    pseudo = our_ip + to_ip + struct.pack("!BBH", 0, 17, length)
    # A checksum that comes out 0 is sent as all ones: 0 says there is none.
    udp = udp[:6] + struct.pack("!H", checksum(pseudo + udp) or 0xFFFF) + udp[8:]
    ip = struct.pack("!BBHHHBBH", 0x45, 0, 20 + length, seq & 0xFFFF, 0x4000, 64, 17, 0) + our_ip + to_ip
    ip = ip[:10] + struct.pack("!H", checksum(ip)) + ip[12:]
    return pad(to_mac + OUR_MAC + struct.pack("!H", ETH_IPV4) + ip + udp)


def udp_reply(frame: bytes, our_ip: bytes, from_ip: bytes, port: int):
    """What the UDP datagram `frame` is carries, or None when it is not one
    from `port` for us whose checksums hold."""
    if len(frame) < 42 or frame[12:14] != struct.pack("!H", ETH_IPV4):
        return None
    ip = frame[14:]
    header = (ip[0] & 0xF) * 4
    total = struct.unpack("!H", ip[2:4])[0]
    if ip[0] >> 4 != 4 or ip[9] != 17 or ip[12:16] != from_ip or ip[16:20] != our_ip:
        return None
    if total > len(ip) or checksum(ip[:header]) != 0:
        return None
    udp = ip[header:total]
    if len(udp) < 8:
        return None
    src, dst, length, check = struct.unpack("!HHHH", udp[:8])
    if src != port or dst != OUR_PORT or length != len(udp):
        return None
    if check and checksum(from_ip + our_ip + struct.pack("!BBH", 0, 17, length) + udp) != 0:
        return None
    return udp[8:]


class Wire:
    """The other end of the card."""

    def __init__(self, listen: int, send: int, our_ip: bytes):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", listen))
        self.to = ("127.0.0.1", send)
        self.our_ip = our_ip

    def send(self, frame: bytes):
        self.sock.sendto(frame, self.to)

    def frame(self, until: float):
        """The next frame from the card, or None when `until` comes first.
        Whoever asks for our address meanwhile is told."""
        while True:
            left = until - time.monotonic()
            if left <= 0:
                return None
            self.sock.settimeout(left)
            try:
                frame = self.sock.recv(4096)
            except socket.timeout:
                return None
            if frame[12:14] == struct.pack("!H", ETH_ARP) and len(frame) >= 42:
                op = struct.unpack("!H", frame[20:22])[0]
                if op == 1 and frame[38:42] == self.our_ip:
                    self.send(arp(2, frame[6:12], frame[22:28], self.our_ip, frame[28:32]))
                    continue
            return frame

    def who_has(self, ip: bytes, patience: float):
        """The address of the card that has `ip`, asked every half second
        for `patience` seconds; None when nobody says."""
        end = time.monotonic() + patience
        while time.monotonic() < end:
            self.send(arp(1, BROADCAST, b"\0" * 6, self.our_ip, ip))
            until = min(end, time.monotonic() + 0.5)
            while (frame := self.frame(until)) is not None:
                if (
                    frame[12:14] == struct.pack("!H", ETH_ARP)
                    and struct.unpack("!H", frame[20:22])[0] == 2
                    and frame[28:32] == ip
                ):
                    return frame[22:28]
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("address", help="the guest's address")
    ap.add_argument("--from", dest="ours", default=None, help="our address (default: the guest's network, host 1)")
    ap.add_argument("--listen", type=int, default=5700, help="the port the card sends to (default 5700)")
    ap.add_argument("--send", type=int, default=5701, help="the port the card listens on (default 5701)")
    ap.add_argument("-c", "--count", type=int, default=1000, help="pings to send (default 1000)")
    ap.add_argument("-s", "--size", type=int, default=56, help="bytes each carries (default 56, at most 1472)")
    ap.add_argument("--udp", type=int, default=None, metavar="PORT", help="send UDP datagrams to an echo server on this port, not ICMP echo (4 bytes each at least)")
    ap.add_argument("-w", "--window", type=int, default=1, help="pings in flight at once (default 1)")
    ap.add_argument("--warmup", type=int, default=50, help="pings sent first and not counted (default 50)")
    ap.add_argument("--patience", type=float, default=30.0, help="seconds to wait for the guest to have the address")
    ap.add_argument("--timeout", type=float, default=1.0, help="seconds after which a ping is lost")
    args = ap.parse_args()
    if not 0 <= args.size <= 1472 or args.count < 1 or args.window < 1:
        ap.error("a size from 0 to 1472, and at least one ping and one in flight")
    if args.udp is not None and (not 0 < args.udp < 65536 or args.size < 4):
        ap.error("a port from 1 to 65535, and datagrams of 4 bytes at least")

    guest = socket.inet_aton(args.address)
    ours = socket.inet_aton(args.ours) if args.ours else guest[:3] + b"\x01"
    wire = Wire(args.listen, args.send, ours)
    mac = wire.who_has(guest, args.patience)
    if mac is None:
        print(f"udp_ping: nobody has {args.address}")
        return 2
    print(f"{args.address} is at {mac.hex(':')}")

    ident = 0x4543
    data = bytes(i & 0xFF for i in range(args.size))

    # A UDP datagram has no sequence number, so its first four bytes are
    # one: which of the datagrams in flight an echo is.
    def request(seq: int) -> bytes:
        if args.udp is None:
            return echo_request(mac, ours, guest, ident, seq, data)
        return udp_datagram(mac, ours, guest, args.udp, seq, struct.pack("!I", seq) + data[4:])

    def answer(frame: bytes):
        """The sequence number `frame` answers, or None when it answers none."""
        if args.udp is None:
            reply = echo_reply(frame, ours, guest, ident)
            return reply[0] if reply is not None and reply[1] == data else None
        got = udp_reply(frame, ours, guest, args.udp)
        if got is None or len(got) != len(data) or got[4:] != data[4:]:
            return None
        return struct.unpack("!I", got[:4])[0]

    def run(count: int):
        """`count` pings, `window` in flight: the round trips in ns of the
        ones answered, how many were lost, and how long it all took."""
        sent_at = {}
        trips = []
        lost = 0
        next_seq = 0
        start = time.perf_counter_ns()
        while next_seq < count or sent_at:
            while next_seq < count and len(sent_at) < args.window:
                wire.send(request(next_seq & 0xFFFF))
                sent_at[next_seq & 0xFFFF] = time.perf_counter_ns()
                next_seq += 1
            oldest = min(sent_at.values())
            frame = wire.frame(time.monotonic() + args.timeout - (time.perf_counter_ns() - oldest) / 1e9)
            now = time.perf_counter_ns()
            if frame is None:
                # The oldest ran out of time, and whatever is as old.
                for seq in [s for s, t in sent_at.items() if now - t >= args.timeout * 1e9]:
                    del sent_at[seq]
                    lost += 1
                continue
            seq = answer(frame)
            if seq in sent_at:
                trips.append(now - sent_at.pop(seq))
        return trips, lost, time.perf_counter_ns() - start

    if args.warmup:
        run(args.warmup)
    trips, lost, took = run(args.count)
    what = "pings" if args.udp is None else f"datagrams to port {args.udp}"
    print(f"{args.count} {what} of {args.size} bytes, {args.window} in flight: {len(trips)} answered, {lost} lost, {took / 1e9:.3f} s")
    if trips:
        us = sorted(t / 1000 for t in trips)
        nth = lambda p: us[min(len(us) - 1, int(len(us) * p))]
        print(
            f"round trip, us: min {us[0]:.0f}  median {statistics.median(us):.0f}  mean {statistics.fmean(us):.0f}"
            f"  p90 {nth(0.90):.0f}  p99 {nth(0.99):.0f}  max {us[-1]:.0f}"
        )
        print(f"{'pings' if args.udp is None else 'datagrams'} a second: {len(trips) / (took / 1e9):.0f}")
    return 1 if lost else 0


if __name__ == "__main__":
    sys.exit(main())
