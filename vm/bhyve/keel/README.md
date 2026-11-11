# keelOS's additions to bhyve

Copies of files from keelOS's bhyve (keelOS `root/bhyve`, commit `db60789fb881fac87033daf7c368973070bd2954`), the ones this port builds with
(`../build.sh`):

| File | What it adds |
|---|---|
| `gen-usbkbd.py` | a USB keyboard for the xHCI (`-s N,xhci,tablet,kbd`), generated from FreeBSD's `usb_mouse.c`: upstream bhyve has only PS/2 keyboards, and arm64 has no controller for those |
| `patch-usbmouse.py` | the tablet queues every click and wheel step |
| `patch-usbemul.py` | asynchronous USB devices, 256 blocks per TD |
| `patch-xhci.py` | several interrupters (`-o keel.xhci_intrs=N`), control transfer and LINK TRB fixes, remote ports |
| `usb_remote.c`, `usb_remote.h` | remote USB devices on the xHCI's free ports (`-o keel.usb_sock=PATH`) |
| `pci_virtio_gpu.c`, `pci_virtio_gpu.h` | virtio-gpu 2D, the picture in a shared memory object |
| `audio.c`, `audio.h`, `hda_codec.c`, `pci_hda.c`, `pci_hda.h` | the HD Audio card (`-s N,hda,play=keel:PATH`): what the guest plays goes into a ring another process reads (16-bit stereo; layout in `audio.h`). They replace FreeBSD's files whole. From keelOS `root/bhyve/hda` at commit `19e813ff01a4c679b7c649970bdb7668be23df30`; `patches/keel/0003` lets the ring be a file |

The `.py` files are generators: each takes FreeBSD's source file and prints keelOS's, changing it at
anchors (pieces of the original text) and stopping when one is gone, so they say at once when a new
FreeBSD no longer fits.

License: BSD-2-Clause, Copyright (c) 2026 keelOS (the text is in `usb_remote.c`).
To take a newer keelOS: copy the files again and write its commit here.
