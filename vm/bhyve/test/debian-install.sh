#!/bin/sh
# debian-install.sh ISO TARGET: install Debian from its netinst ISO onto the disk image TARGET with
# nobody at the keyboard (answers: debian-preseed.cfg, served to the guest over NAT from this Mac),
# then boot TARGET and check it. Needs $UBOOT (see run.sh). Takes 10 to 20 minutes.
set -e
D=$(cd "$(dirname "$0")/.." && pwd)
iso=$1; target=$2
[ -f "$iso" ] && [ -n "$target" ] || { echo "usage: debian-install.sh ISO TARGET" >&2; exit 2; }
rm -f "$target"; mkfile -n 16g "$target"

# the installer fetches its answers from the host side of vmnet's NAT network
python3 -m http.server 8017 --directory "$D/test" >/dev/null 2>&1 &
http=$!
trap 'kill $http 2>/dev/null' EXIT

# The ISO twice: as the first virtio disk for U-Boot and GRUB to boot from (this U-Boot boots only
# "virtio 0" and has no AHCI driver), and as a CD for the installer, which looks for its medium on
# CD drives. The target is then the second virtio disk, /dev/vdb, while installing.
BHYVE_ARGS="-s 2,virtio-blk,$target -s 3,ahci-cd,$iso -s 4,virtio-net,nat" \
"$D/test/run.sh" "$iso,ro" 4 -- \
    '120:GNU GRUB=>' 'sleep:1' '=><key>c' \
    '20:grub>=>linux /install.a64/vmlinuz auto=true priority=critical preseed/url=http://192.168.64.1:8017/debian-preseed.cfg ---' \
    '20:grub>=>initrd /install.a64/initrd.gz' \
    '60:grub>=>boot' \
    '3000:reboot: Power down=>' 'sleep:5' || true

echo "=== installed; booting from the disk"
BHYVE_ARGS="-s 4,virtio-net,nat" \
"$D/test/run.sh" "$target" 4 -- \
    '180:login:=>root' '20:Password:=>bhyve1' \
    '30:# =>hostname; nproc; df -h / | tail -1; ip -4 -br addr | grep -v "^lo"; dpkg -l | grep -c "^ii"; which fio iperf3; echo fin1' \
    '30:fin1=>poweroff' 'sleep:20'
