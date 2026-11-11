#!/usr/bin/env python3
"""vnc.py PORT ACTION...: a minimal VNC (RFB 3.8, no authentication) client for testing bhyve's fbuf.

Actions:
  info             print the server's size and name
  pixels X,Y,N     print N pixels starting at (X,Y) as RRGGBB, from a full raw update
  ppm:FILE         save the screen as a PPM file
  move:N           send N pointer moves along a diagonal (the guest's USB tablet gets them)
  type:TEXT        press and release each character's key (\n is Enter)
  keys:K1 K2+K3    named keys and chords in order: enter esc tab win ctrl alt shift f1..f12, e.g. keys:win+r
  click:X,Y        a left click at that pixel
"""
import socket, struct, sys, time

def rd(s, n):
    b = b""
    while len(b) < n:
        d = s.recv(n - len(b))
        if not d: sys.exit("vnc.py: the server closed the connection")
        b += d
    return b

port = int(sys.argv[1])
deadline = time.time() + 10
while True:
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=10); break
    except OSError:
        if time.time() > deadline: sys.exit("vnc.py: nothing listens on port %d" % port)
        time.sleep(0.2)
ver = rd(s, 12); s.sendall(b"RFB 003.008\n")
n = rd(s, 1)[0]; types = rd(s, n)
if 1 not in types: sys.exit("vnc.py: the server wants authentication (types %r)" % list(types))
s.sendall(b"\x01")
if struct.unpack("!I", rd(s, 4))[0] != 0: sys.exit("vnc.py: security handshake failed")
s.sendall(b"\x01")                                   # ClientInit: shared
w, h = struct.unpack("!HH", rd(s, 4)); pf = rd(s, 16)
name = rd(s, struct.unpack("!I", rd(s, 4))[0]).decode()
# 32 bpp, depth 24, little-endian true colour, R at bit 16, G at 8, B at 0; raw encoding only
s.sendall(struct.pack("!BxxxBBBBHHHBBBxxx", 0, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
s.sendall(struct.pack("!BxHi", 2, 1, 0))

def screen():
    s.sendall(struct.pack("!BBHHHH", 3, 0, 0, 0, w, h))
    fb = bytearray(w * h * 4)
    while True:
        t = rd(s, 1)[0]
        if t == 0:
            rd(s, 1); nrect = struct.unpack("!H", rd(s, 2))[0]
            for _ in range(nrect):
                x, y, rw, rh, enc = struct.unpack("!HHHHi", rd(s, 12))
                if enc != 0: sys.exit("vnc.py: encoding %d, asked for raw" % enc)
                data = rd(s, rw * rh * 4)
                for row in range(rh):
                    o = ((y + row) * w + x) * 4
                    fb[o:o + rw * 4] = data[row * rw * 4:(row + 1) * rw * 4]
            return fb
        elif t == 2: pass                             # bell
        elif t == 3: rd(s, 3); rd(s, struct.unpack("!I", rd(s, 4))[0])
        else: sys.exit("vnc.py: message type %d" % t)

for act in sys.argv[2:]:
    if act == "info":
        print("vnc: %dx%d %r (%s)" % (w, h, name, ver.decode().strip()))
    elif act.startswith("pixels"):
        x, y, n = map(int, act.split(None, 1)[1].split(",")) if " " in act else map(int, act[7:].split(","))
        fb = screen(); o = (y * w + x) * 4
        print("vnc pixels:", " ".join("%02x%02x%02x" % (fb[o + i * 4 + 2], fb[o + i * 4 + 1], fb[o + i * 4]) for i in range(n)))
    elif act.startswith("ppm:"):
        fb = screen()
        with open(act[4:], "wb") as f:
            f.write(b"P6 %d %d 255\n" % (w, h))
            f.write(bytes(b for i in range(0, len(fb), 4) for b in (fb[i + 2], fb[i + 1], fb[i])))
        print("vnc: saved", act[4:])
    elif act.startswith("move:"):
        n = int(act[5:])
        for i in range(n):
            s.sendall(struct.pack("!BBHH", 5, 0, (i * 7) % w, (i * 5) % h)); time.sleep(0.01)
        print("vnc: sent %d pointer moves" % n)
    elif act.startswith("keys:"):
        # keys:win+r enter tab esc ...: named keys and chords, in order
        NAMES = {"enter": 0xff0d, "esc": 0xff1b, "tab": 0xff09, "bs": 0xff08, "del": 0xffff, "space": 0x20,
                 "up": 0xff52, "down": 0xff54, "left": 0xff51, "right": 0xff53, "home": 0xff50, "end": 0xff57,
                 "win": 0xffeb, "ctrl": 0xffe3, "alt": 0xffe9, "shift": 0xffe1}
        NAMES.update({"f%d" % i: 0xffbd + i for i in range(1, 13)})
        for chord in act[5:].split():
            ks = [NAMES[k] if k in NAMES else ord(k) for k in chord.split("+")]
            for k in ks:
                s.sendall(struct.pack("!BBxxI", 4, 1, k)); time.sleep(0.05)
            for k in reversed(ks):
                s.sendall(struct.pack("!BBxxI", 4, 0, k)); time.sleep(0.05)
            time.sleep(0.3)
        print("vnc: sent", act[5:])
    elif act.startswith("click:"):
        # click:X,Y: move there and press the left button
        x, y = map(int, act[6:].split(","))
        for b in (0, 1, 0):
            s.sendall(struct.pack("!BBHH", 5, b, x, y)); time.sleep(0.15)
        print("vnc: clicked", x, y)
    elif act.startswith("type:"):
        # A keyboard has no key for "A" or "|": the character's unshifted key with Shift held
        # (US layout), as a VNC viewer sends it.
        SHIFTED = dict(zip('~!@#$%^&*()_+{}|:"<>?', "`1234567890-=[]\\;',./"))
        for ch in act[5:]:
            shift = ch in SHIFTED or ch.isupper()
            k = 0xff0d if ch == "\n" else ord(SHIFTED.get(ch, ch.lower()))
            if shift:
                s.sendall(struct.pack("!BBxxI", 4, 1, 0xffe1)); time.sleep(0.02)
            for down in (1, 0):
                s.sendall(struct.pack("!BBxxI", 4, down, k)); time.sleep(0.02)
            if shift:
                s.sendall(struct.pack("!BBxxI", 4, 0, 0xffe1)); time.sleep(0.02)
        print("vnc: typed %d keys" % len(act[5:]))
    else:
        sys.exit(__doc__)
