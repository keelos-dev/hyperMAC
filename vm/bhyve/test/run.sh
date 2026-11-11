#!/bin/sh
# run.sh DISK CPUS -- STEP...: boot DISK under build/bhyve with U-Boot ($UBOOT) and drive its serial
# console with console.py's STEPs. Prints the console and then bhyve's own output and exit status
# (0 = the guest rebooted, 1 = powered off, 4 = bhyve failed).
D=$(cd "$(dirname "$0")/.." && pwd)
disk=$1; cpus=$2; shift 3
: "${UBOOT:?UBOOT=path of u-boot.bin (FreeBSD package u-boot-bhyve-arm64)}"
mkdir -p "$D/build/run"
python3 "$D/test/console.py" --pty "'$D/build/bhyve' -c $cpus -m ${MEM:-2G} -o console={tty} -o rundir='$D/build/run' \
    -o bootrom='$UBOOT' -s 0,hostbridge -s 1,virtio-blk,'$disk' $BHYVE_ARGS vm$$ > '$D/build/run.log' 2>&1; \
    echo \"bhyve exit status \$?\" >> '$D/build/run.log'" "$@"
st=$?
sleep 1
pkill -f "build/bhyve .*vm$$\$" 2>/dev/null
echo "--- bhyve:"
cat "$D/build/run.log"
exit $st
