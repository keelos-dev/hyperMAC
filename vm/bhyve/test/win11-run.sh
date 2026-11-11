#!/bin/sh
# win11-run.sh DIR [bhyve arguments]: run the Windows VM in DIR (made by win11-install.sh) until it
# powers off; a guest reboot (bhyve exits 0) starts it again. The picture: VNC on 127.0.0.1:$VNC
# (default 5910); the serial console goes to DIR/serial.log, bhyve's own messages to DIR/bhyve.log.
# `kill -TERM` of the bhyve process is the power button.
#   CPUS=4 MEM=6G        the VM's size
#   FBW=1024 FBH=768     the screen
#   CD=file.iso          a CD to attach (the install medium, virtio-win.iso)
#   CD2=file.iso         a second one
#   CDKEY=1              press a key at "Press any key to boot from CD or DVD" (once)
B=$(cd "$(dirname "$0")/.." && pwd)
dir=${1:?usage: win11-run.sh DIR [bhyve arguments]}; shift
dir=$(cd "$dir" && pwd)
cds=
[ -n "$CD" ] && cds="$cds -s 3,ahci-cd,'$CD',ro"
[ -n "$CD2" ] && cds="$cds -s 4,ahci-cd,'$CD2',ro"
n=0
while :; do
	n=$((n + 1))
	cmd="'$B/build/bhyve' -c ${CPUS:-4} -m ${MEM:-6G} -o console={tty} -o rtc.use_localtime=true -o rundir='$dir' \
	    -o bootrom='$B/firmware/QEMU_EFI.fd' -o bootvars='$dir/vars.fd' \
	    -s 0,hostbridge -s 2,nvme,'$dir/disk.img' $cds -s 5,xhci,tablet,kbd -s 6,virtio-rnd \
	    -s 7,fbuf,rfb=127.0.0.1:${VNC:-5910},w=${FBW:-1024},h=${FBH:-768} $* win > '$dir/bhyve.log' 2>&1; \
	    echo \"bhyve exit status \$?\" >> '$dir/bhyve.log'"
	if [ -n "$CDKEY" ] && [ $n = 1 ]; then
		python3 "$B/test/console.py" --log "$dir/serial.log" --pty "$cmd" \
		    '60:SATA DVD ROM=>' 'sleep:0.3' '=><enter>' 'sleep:0.5' '=><enter>' 'sleep:0.5' '=><enter>' \
		    'sleep:1' '=><enter>' 'sleep:1' '=><enter>' 'sleep:1' '=><enter>' '864000:NEVER-PRINTED=>' > /dev/null 2>&1
	else
		python3 "$B/test/console.py" --log "$dir/serial.log" --pty "$cmd" '864000:NEVER-PRINTED=>' > /dev/null 2>&1
	fi
	st=$(tail -1 "$dir/bhyve.log")
	echo "$(date '+%F %T') run $n: $st" >> "$dir/runs.log"
	[ "$st" = "bhyve exit status 0" ] || break
	sleep 2
done
echo "$st"
[ "$st" = "bhyve exit status 1" ]
