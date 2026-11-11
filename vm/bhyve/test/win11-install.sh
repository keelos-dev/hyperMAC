#!/bin/sh
# win11-install.sh ISO DIR: install Windows 11 arm64 from Microsoft's ISO into a new VM in DIR, with
# nobody at the keyboard: test/win11-autounattend.xml answers setup from a second CD (the whole
# 40 GB NVMe disk, user win / bhyve1, logs on by itself). Three boots, about ten minutes; the guest
# powers off at its first desktop. Then: CD=virtio-win.iso test/win11-run.sh DIR -s 8,virtio-net,nat
# Watch: a VNC viewer on 127.0.0.1:5910 (VNC=port), or python3 test/vnc.py 5910 ppm:shot.ppm
set -e
B=$(cd "$(dirname "$0")/.." && pwd)
iso=${1:?usage: win11-install.sh ISO DIR}; dir=${2:?usage: win11-install.sh ISO DIR}
[ -f "$iso" ] || { echo "$iso: no such file" >&2; exit 2; }
[ ! -e "$dir/disk.img" ] || { echo "$dir/disk.img exists: this would overwrite a VM" >&2; exit 2; }
mkdir -p "$dir/cd"
cp "$B/test/win11-autounattend.xml" "$dir/cd/autounattend.xml"
hdiutil makehybrid -quiet -iso -joliet -default-volume-name UNATTEND -o "$dir/unattend.iso" "$dir/cd"
mkfile -n "${DISK:-40g}" "$dir/disk.img"
echo "installing: $(date '+%T')"
CD="$iso" CD2="$dir/unattend.iso" CDKEY=1 "$B/test/win11-run.sh" "$dir"
echo "installed:  $(date '+%T')"
cat "$dir/runs.log"
