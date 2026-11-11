#!/usr/bin/env python3
"""netpeer.py COMMAND...: a stand-in for the network, for testing a NIC without root.

Makes a datagram socketpair, runs COMMAND with one end inherited (its number is in $NETFD: give it
to bhyve as `-s N,virtio-net,dgram,fd=$NETFD`), and on the other end plays host 10.0.0.1: answers
ARP requests for 10.0.0.1 and ICMP echo requests sent to it. The guest uses 10.0.0.2/24.
At the end prints how many frames went each way; exits with COMMAND's status.
"""
import os, socket, struct, subprocess, sys, threading

MAC = bytes.fromhex("02005e000001")
IP = socket.inet_aton("10.0.0.1")
a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
for s in (a, b):
    s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 2 << 20)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 2 << 20)
stats = {"rx": 0, "tx": 0, "arp": 0, "echo": 0, "rx_bytes": 0, "largest": 0}

def csum(d):
    if len(d) & 1: d += b"\0"
    t = sum(struct.unpack("!%dH" % (len(d) // 2), d))
    while t >> 16: t = (t & 0xffff) + (t >> 16)
    return ~t & 0xffff

def serve():
    while True:
        try:
            f = a.recv(65536)
        except OSError:
            return
        stats["rx"] += 1; stats["rx_bytes"] += len(f); stats["largest"] = max(stats["largest"], len(f))
        if len(f) < 42: continue
        src, etype = f[6:12], f[12:14]
        reply = None
        if etype == b"\x08\x06" and f[20:22] == b"\x00\x01" and f[38:42] == IP:      # ARP request for us
            reply = src + MAC + etype + f[14:20] + b"\x00\x02" + MAC + IP + f[22:32]
            stats["arp"] += 1
        elif etype == b"\x08\x00" and f[23] == 1 and f[30:34] == IP:                # ICMP to us
            ihl = (f[14] & 0xf) * 4
            ip, icmp = bytearray(f[14:14 + ihl]), bytearray(f[14 + ihl:14 + struct.unpack("!H", f[16:18])[0]])
            if icmp[0] != 8: continue
            icmp[0] = 0; icmp[2:4] = b"\0\0"; icmp[2:4] = struct.pack("!H", csum(bytes(icmp)))
            ip[12:16], ip[16:20] = ip[16:20], ip[12:16]
            ip[10:12] = b"\0\0"; ip[10:12] = struct.pack("!H", csum(bytes(ip)))
            reply = src + MAC + etype + bytes(ip) + bytes(icmp)
            stats["echo"] += 1
        if reply:
            try:
                a.send(reply); stats["tx"] += 1
            except OSError:
                pass

threading.Thread(target=serve, daemon=True).start()
env = dict(os.environ, NETFD=str(b.fileno()))
st = subprocess.call(sys.argv[1:], pass_fds=[b.fileno()], env=env)
print("netpeer: from guest %(rx)d frames (%(rx_bytes)d bytes, largest %(largest)d), to guest %(tx)d; "
      "ARP replies %(arp)d, echo replies %(echo)d" % stats)
sys.exit(st)
