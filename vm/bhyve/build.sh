#!/bin/sh
# build.sh [make arguments]: FreeBSD's sources (freebsd/, unchanged) + patches/ -> build/src, then make.
# The result is build/bhyve, signed ad hoc with the hypervisor entitlement.
set -e
D=$(cd "$(dirname "$0")" && pwd)
cd "$D"
mkdir -p build/include
ln -sfn ../../build/src/sys/arm64/include build/include/machine

# build/src.new = freebsd/ with the patches on top; build/src takes only the files whose content
# changed, so make recompiles just those.
rm -rf build/src.new
mkdir -p build/src.new build/src
rsync -a freebsd/ build/src.new/
# One series: the patches of both directories in the order of their numbers (a later one may
# change a file an earlier one of the other kind changed).
for p in $(ls patches/upstream/*.patch patches/macos/*.patch | awk -F/ '{ print $NF "\t" $0 }' | sort | cut -f2); do
	patch -d build/src.new -p1 -s -N --no-backup-if-mismatch < "$p" ||
	    { echo "build.sh: $p does not apply" >&2; exit 1; }
done
# keelOS's additions (keel/README.md): generators that edit FreeBSD's files by anchors, and whole files.
K=keel
N=build/src.new/usr.sbin/bhyve
# a USB keyboard for the xHCI ("-s N,xhci,tablet,kbd"), generated from the tablet: upstream has
# only PS/2 keyboards, which arm64 has no controller for
python3 "$K/gen-usbkbd.py" "$N/usb_mouse.c" > "$N/usb_kbd.c"
# A generator takes FreeBSD's file and prints keel's; it stops if one of its anchors is gone.
keel_gen() { # generator, file
	python3 "$K/$1" "$N/$2" > "$N/$2.keel" || { echo "build.sh: $1 does not fit $2 any more" >&2; exit 1; }
	mv "$N/$2.keel" "$N/$2"
}
keel_gen patch-usbmouse.py usb_mouse.c	# the tablet queues every click and wheel step
keel_gen patch-usbemul.py usb_emul.h	# asynchronous USB devices, 256 blocks per TD
keel_gen patch-xhci.py pci_xhci.c	# several interrupters (-o keel.xhci_intrs=N), control transfer and LINK TRB fixes, remote ports
cp "$K/usb_remote.c" "$K/usb_remote.h" "$N/"	# remote USB devices on the xHCI's free ports (-o keel.usb_sock=PATH)
cp "$K/pci_virtio_gpu.c" "$K/pci_virtio_gpu.h" "$N/"	# virtio-gpu 2D, the picture in a shared memory object

# Our own changes to what the generators made (patches/keel): they apply to keelOS's text, so
# they come last. mkpatch.sh keel NAME FILE makes one.
for p in $(ls patches/keel/*.patch 2>/dev/null); do
	patch -d build/src.new -p1 -s -N --no-backup-if-mismatch < "$p" ||
	    { echo "build.sh: $p does not apply" >&2; exit 1; }
done

# (no -t: a file that is replaced gets the current time, so it is newer than its object file even when
# the replacement is the older, unedited text)
rsync -rlp --checksum --delete build/src.new/ build/src/

make -j"$(sysctl -n hw.ncpu)" "$@"
