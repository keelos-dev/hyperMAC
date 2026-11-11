#!/usr/bin/env python3
"""kgpu.py VMNAME [FILE.ppm | debug=N]: read the picture of keel's virtio-gpu (-s N,virtio-gpu) from its
shared memory object /keel-gpu-VMNAME: prints the header, counts distinct colours, optionally
saves a PPM. debug=N sets the device's log level (1: every command, 2: also the queues' state twice a
second; into bhyve's stderr)."""
import ctypes, mmap, os, struct, sys

name = "/keel-gpu-" + sys.argv[1]
libc = ctypes.CDLL(None, use_errno=True)
libc.shm_open.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
dbg = [a for a in sys.argv[2:] if a.startswith("debug=")]
fd = libc.shm_open(name.encode(), os.O_RDWR if dbg else os.O_RDONLY, 0)
if fd < 0: sys.exit("kgpu.py: %s: %s" % (name, os.strerror(ctypes.get_errno())))
PIXELS = 4096
m = mmap.mmap(fd, PIXELS + 3840 * 2160 * 4 + 64 * 64 * 4, prot=mmap.PROT_READ | (mmap.PROT_WRITE if dbg else 0))
if dbg:
    struct.pack_into("<I", m, 64, int(dbg[0][6:]))
    print("kgpu: debug level", dbg[0][6:]); sys.exit(0)
magic, seq, active, w, h = struct.unpack_from("<5I", m, 0)
print("kgpu: magic %#x seq %u active %u %ux%u" % (magic, seq, active, w, h))
if active and w and h:
    pix = m[PIXELS:PIXELS + w * h * 4]
    colours = {pix[i:i + 3] for i in range(0, len(pix), 4 * 37)}
    lit = sum(1 for i in range(0, len(pix), 4 * 37) if pix[i:i + 3] != b"\0\0\0")
    print("kgpu: %d distinct colours in a sample, %d of %d sampled pixels not black" % (len(colours), lit, len(pix) // (4 * 37)))
    if len(sys.argv) > 2:
        with open(sys.argv[2], "wb") as f:
            f.write(b"P6 %d %d 255\n" % (w, h))
            f.write(bytes(b for i in range(0, len(pix), 4) for b in (pix[i + 2], pix[i + 1], pix[i])))
        print("kgpu: saved", sys.argv[2])
