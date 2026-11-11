#!/bin/sh
# Build hyperMAC on the Mac: sh build.sh  ->  target/release/hypermac
# IronRDP comes from vendor/IronRDP (tools/vendor-ironrdp.sh: keel-rdp's pin + ironrdp-keel.patch).
# Needs rust (rustup, stable) and git; Xcode Command Line Tools for the frameworks.
set -e
D=$(cd "$(dirname "$0")" && pwd)
sh "$D/tools/vendor-ironrdp.sh"
cd "$D" && cargo build --release

# Sign with a stable identity so the TCC grants (Screen Recording, Accessibility) survive a
# rebuild: TCC keys on identifier + certificate, while an ad-hoc signature changes with every
# build. MACRDP_SIGN_IDENTITY overrides; default: the first "Apple Development" identity in
# the keychain (Xcode's). None found: left ad-hoc (works, but re-grant after each build).
ID=${MACRDP_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' | head -1)}
sign() {
	if [ -n "$ID" ]; then
		codesign --force --sign "$ID" --identifier dev.keelos.hypermac --timestamp=none "$1" && echo "signed $1 as: $ID"
	else
		echo "no Apple Development identity: $1 stays ad-hoc (TCC grants will not survive a rebuild)"
	fi
}
sign target/release/hypermac

# The VM program (vm/): one detached `hypermac-vm run NAME` process per VM, started from the
# main window's Virtual Machines tab. Virtualization.framework needs its entitlement.
(cd "$D/vm" && cargo build --release)
VMBIN="$D/vm/target/release/hypermac"
sign_vm() {
	codesign --force --sign "${ID:--}" --identifier dev.keelos.hypermac.vm --timestamp=none \
		--entitlements "$D/vm/vz.entitlements" "$1" && echo "signed $1 (virtualization)"
}
sign_vm "$VMBIN"

# The LaunchAgent runs its own copy (hypermac install puts it in <state>/bin): refresh it when
# it exists, then restart the service. Building never touches the running copy.
STATE="${MACRDP_STATE:-$HOME/Library/Application Support/hyperMAC}"
INST="$STATE/bin/hypermac"
if [ -f "$INST" ]; then
	cp target/release/hypermac "$INST.new" && sign "$INST.new" >/dev/null && mv -f "$INST.new" "$INST" \
		&& echo "installed copy refreshed: $INST" \
		&& echo "restart it with:  launchctl kickstart -k gui/$(id -u)/dev.keelos.hypermac"
	# the menu bar's copy, with a bundle id of its own (main.rs write_menu_binary)
	"$INST" menu-binary --state "$STATE" && echo "menu bar copy refreshed: $INST-menu" \
		&& echo "restart it with:  launchctl kickstart -k gui/$(id -u)/dev.keelos.hypermac.menu"
fi
# efiprobe (vm/tools/efiprobe): the EFI program a Windows VM boots through (vm/src/winsetup.rs)
(cd "$D/vm/tools/efiprobe" && cargo build --release --target aarch64-unknown-uefi)
EFIPROBE="$D/vm/tools/efiprobe/target/aarch64-unknown-uefi/release/efiprobe.efi"

# The bhyve engine (vm/bhyve): what Windows VMs run on (vm/src/bhyve.rs). Its own build.sh makes
# build/bhyve (ad-hoc signed there). If it does not build, Windows stays
# on the old Virtualization.framework + efiprobe route.
BHYVE=
if (cd "$D/vm/bhyve" && ./build.sh > build.log 2>&1); then
	BHYVE="$D/vm/bhyve/build/bhyve"
else
	echo "vm/bhyve did not build (vm/bhyve/build.log): Windows VMs stay on Virtualization.framework"
fi

# the VM program beside it (a new file moved into place: running VMs keep the old one)
if [ -d "$(dirname "$INST")" ]; then
	if [ -n "$BHYVE" ]; then
		B="$(dirname "$INST")/bhyve"
		cp "$BHYVE" "$B.new" && codesign --force --sign "${ID:--}" --identifier dev.keelos.hypermac.bhyve --timestamp=none \
			--entitlements "$D/vm/bhyve/hvf/bhyve.entitlements" "$B.new" && mv -f "$B.new" "$B" \
			&& cp "$D/vm/bhyve/firmware/QEMU_EFI.fd" "$(dirname "$INST")/QEMU_EFI.fd" \
			&& cp "$D/vm/bhyve/firmware/QEMU_VARS.secboot.fd" "$(dirname "$INST")/QEMU_VARS.secboot.fd" \
			&& echo "bhyve engine installed: $B (hypervisor, virtualization)"
	fi
	# The 3D engine for Windows guests (vm.json "gpu3d"), when vm/bhyve/gpu3d.sh has made it: bhyve-3d
	# and the folder it looks in beside itself. Not built here: it needs LLVM 15, meson and ten minutes.
	G3="$D/vm/bhyve/build"
	if [ -n "$BHYVE" ] && [ -x "$G3/bhyve-3d" ] && [ -f "$G3/gpu3d/virgl_render_server" ]; then
		B="$(dirname "$INST")/bhyve-3d"
		rm -rf "$(dirname "$INST")/gpu3d.new" && cp -R "$G3/gpu3d" "$(dirname "$INST")/gpu3d.new" \
			&& for f in "$(dirname "$INST")/gpu3d.new"/*.dylib "$(dirname "$INST")/gpu3d.new/virgl_render_server"; do
				codesign --force --sign "${ID:--}" --timestamp=none "$f" || exit 1
			done \
			&& rm -rf "$(dirname "$INST")/gpu3d" && mv "$(dirname "$INST")/gpu3d.new" "$(dirname "$INST")/gpu3d" \
			&& cp "$G3/bhyve-3d" "$B.new" && codesign --force --sign "${ID:--}" --identifier dev.keelos.hypermac.bhyve --timestamp=none \
				--entitlements "$D/vm/bhyve/hvf/bhyve.entitlements" "$B.new" && mv -f "$B.new" "$B" \
			&& echo "3D engine installed: $B, $(dirname "$INST")/gpu3d"
	fi
	cp "$VMBIN" "$INST-vm.new" && sign_vm "$INST-vm.new" >/dev/null && mv -f "$INST-vm.new" "$INST-vm" \
		&& echo "VM program installed: $INST-vm"
	cp "$EFIPROBE" "$(dirname "$INST")/efiprobe.efi" && echo "efiprobe installed: $(dirname "$INST")/efiprobe.efi"
fi
