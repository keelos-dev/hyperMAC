#!/bin/sh
# Release build: sh tools/package.sh  ->  dist/hypermac-<major.minor>-<commit>-<UTC time>-macos-arm64.dmg
# (kRDPclient's naming), signed with Developer ID, notarized and stapled, checked with spctl.
# Upload it and dist/changelog.json to https://github.com/keelos-dev/hyperMAC as a release
# whose tag is the dmg's name without .dmg (README "Release").
#
# Run it in a GUI terminal: over ssh the login keychain is locked and codesign fails with
# errSecInternalComponent. Needs, once (README "Release"):
#   - a "Developer ID Application" certificate in the login keychain
#     (MACRDP_RELEASE_IDENTITY picks one when there are several);
#   - notary credentials stored as the keychain profile "hypermac-notary"
#     (xcrun notarytool store-credentials hypermac-notary ...). No password is in this script.
# It never touches the running service (~/Library/Application Support/hyperMAC, the LaunchAgent).
set -e
D=$(cd "$(dirname "$0")/.." && pwd)
cd "$D"
command -v cargo >/dev/null 2>&1 || . "$HOME/.cargo/env"
die() { echo "package.sh: $*" >&2; exit 1; }

# 1. checks ------------------------------------------------------------------------------
for n in THIRD_PARTY.md LICENSE-IronRDP-MIT THIRD_PARTY-crates.txt THIRD_PARTY-vm-crates.txt Cargo.lock vm/Cargo.lock \
	vm/bhyve/firmware/QEMU_EFI.fd vm/bhyve/firmware/QEMU_VARS.secboot.fd vm/bhyve/firmware/License-edk2.txt \
	vm/bhyve/third-party/LICENSE-libtpms vm/bhyve/third-party/LICENSE-openssl assets/hypermac.icns assets/dmg-background.tiff; do
	[ -f "$n" ] || die "$n is missing"
done
LOCK=$(shasum -a 256 Cargo.lock | cut -d' ' -f1)
head -5 THIRD_PARTY-crates.txt | grep -qx "Cargo.lock sha256: $LOCK" \
	|| die "THIRD_PARTY-crates.txt is not of this Cargo.lock: python3.11 tools/third-party.py, then commit it"
LOCK=$(shasum -a 256 vm/Cargo.lock | cut -d' ' -f1)
head -5 THIRD_PARTY-vm-crates.txt | grep -qx "Cargo.lock sha256: $LOCK" \
	|| die "THIRD_PARTY-vm-crates.txt is not of vm/Cargo.lock: python3.11 tools/third-party.py, then commit it"
COMMIT=$(git rev-parse --short=7 HEAD)
DIRTY=
if [ -n "$(git status --porcelain)" ]; then
	DIRTY="uncommitted changes"
else
	git fetch -q origin || true
	git branch -r --contains HEAD | grep -q 'origin/' || DIRTY="HEAD is not pushed to origin"
fi
[ -z "$DIRTY" ] || COMMIT="$COMMIT-dirty"
VER=$(sed -n 's/^version = "\([0-9]*\)\.\([0-9]*\)\.\([0-9]*\)".*/\1.\2.\3/p' Cargo.toml | head -1)
MM=${VER%.*}
NAME="hypermac-$MM-$COMMIT-$(date -u +%Y%m%dT%H%M%SZ)-macos-arm64"
ID=${MACRDP_RELEASE_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)}
[ -n "$ID" ] || die "no \"Developer ID Application\" identity in the keychain (README \"Release\")"
xcrun notarytool history --keychain-profile hypermac-notary >/dev/null 2>&1 \
	|| die "no notary profile hypermac-notary: xcrun notarytool store-credentials hypermac-notary ..."

# 2. build (arm64 only; Intel is not tested) ------------------------------------------------
sh tools/vendor-ironrdp.sh
# MACRDP_TAG: this build's release name, for "Check for Updates…" (update.rs: newer = another
# commit packed later)
MACRDP_COMMIT=$COMMIT MACRDP_TAG=$NAME cargo build --release --target aarch64-apple-darwin
BIN=target/aarch64-apple-darwin/release/hypermac
# The virtual machines (the window's Virtual Machines tab): the VM program (vm/, one process per
# VM) and the bhyve engine Windows and FreeBSD guests run on (vm/bhyve; needs the keelOS
# repository, vm/bhyve/build.sh). A release without them would show the tab and start nothing.
(cd vm && cargo build --release --target aarch64-apple-darwin)
VMBIN=vm/target/aarch64-apple-darwin/release/hypermac
(cd vm/bhyve && ./build.sh > build.log 2>&1) || die "vm/bhyve did not build (vm/bhyve/build.log)"
BHYVE=vm/bhyve/build/bhyve
# the TPM of a Windows VM is inside bhyve (libtpms, vm/bhyve/Makefile): a release without it
# would install Windows with the hardware checks switched off
grep -q "libtpms needs tpm.path" "$BHYVE" || die "bhyve was built without libtpms (brew install libtpms openssl): no TPM for Windows VMs"
otool -L "$BHYVE" | grep -q homebrew && die "bhyve depends on a Homebrew library: it must link libtpms and libcrypto statically"

# 3. hyperMAC.app ------------------------------------------------------------------------------
OUT=$D/dist
STAGE=$OUT/$NAME
rm -rf "$STAGE" && mkdir -p "$STAGE"
APP=$STAGE/hyperMAC.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/hypermac"
cp assets/hypermac.icns "$APP/Contents/Resources/hypermac.icns"
python3 tools/third-party-vm.py > "$STAGE/THIRD_PARTY-vm.txt"
for n in THIRD_PARTY.md LICENSE-IronRDP-MIT THIRD_PARTY-crates.txt "$STAGE/THIRD_PARTY-vm.txt"; do cp "$n" "$APP/Contents/Resources/"; done
# Programs beside the main one (src/ui/vmui.rs `runner`, vm/src/bhyve.rs `find`), the firmware
# in Resources: it is data, and codesign wants only code in Contents/MacOS.
cp "$VMBIN" "$APP/Contents/MacOS/hypermac-vm"
cp "$BHYVE" "$APP/Contents/MacOS/bhyve"
# The 3D engine for Windows guests (vm.json "gpu3d"; vm/bhyve/gpu3d.sh makes it, it is not built
# here): bhyve-3d with the programs, its folder in Resources (it has the guest's driver and the
# licenses in it besides code; vm/src/bhyve.rs gpu3d() and bhyve-3d's rpath look there).
# A release without it has no 3D; nothing else changes.
GPU3D=
if [ -x vm/bhyve/build/bhyve-3d ] && [ -f vm/bhyve/build/gpu3d/virgl_render_server ]; then
	GPU3D=1
	otool -L vm/bhyve/build/bhyve-3d | grep -q -E "homebrew|/Volumes/|/Users/" && die "bhyve-3d depends on a library outside the app: run vm/bhyve/gpu3d.sh"
	cp vm/bhyve/build/bhyve-3d "$APP/Contents/MacOS/bhyve-3d"
	cp -R vm/bhyve/build/gpu3d "$APP/Contents/Resources/gpu3d"
fi
cp vm/bhyve/firmware/QEMU_EFI.fd "$APP/Contents/Resources/QEMU_EFI.fd"
# the UEFI variables a new Windows VM starts with: Secure Boot on, Microsoft's certificates
cp vm/bhyve/firmware/QEMU_VARS.secboot.fd "$APP/Contents/Resources/QEMU_VARS.secboot.fd"
# LSMinimumSystemVersion 13: ScreenCaptureKit's system audio. Only macOS 26 is tested (README).
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.keelos.hypermac</string>
  <key>CFBundleName</key><string>hyperMAC</string>
  <key>CFBundleDisplayName</key><string>hyperMAC</string>
  <key>CFBundleExecutable</key><string>hypermac</string>
  <key>CFBundleIconFile</key><string>hypermac</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key><array><string>en</string><string>zh-Hans</string></array>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHumanReadableCopyright</key><string>© keelOS</string>
  <key>NSLocalNetworkUsageDescription</key><string>hyperMAC talks to its virtual machines over their network: coherence mode asks Windows where its windows are.</string>
  <key>NSAudioCaptureUsageDescription</key><string>hyperMAC plays this Mac's sound on the remote client and mutes it here meanwhile.</string>
</dict></plist>
PLIST

# 4. sign: hardened runtime. The main program has no entitlements (ScreenCaptureKit, CGEvent,
# VideoToolbox and the sockets need none; add one here only when something fails without it,
# with the reason). The two VM programs first, each with what it cannot run without:
# Virtualization.framework refuses a process without com.apple.security.virtualization,
# Hypervisor.framework one without com.apple.security.hypervisor (bhyve has both: vmnet's
# shared mode takes the first from a process that is not root).
codesign --force --options runtime --timestamp --sign "$ID" --identifier dev.keelos.hypermac.vm \
	--entitlements vm/vz.entitlements "$APP/Contents/MacOS/hypermac-vm"
codesign --force --options runtime --timestamp --sign "$ID" --identifier dev.keelos.hypermac.bhyve \
	--entitlements vm/bhyve/hvf/bhyve.entitlements "$APP/Contents/MacOS/bhyve"
if [ -n "$GPU3D" ]; then
	# (the render server loads libdxmt-native.dylib, signed here like itself: library validation
	# stays on. Someone who builds their own DXMT, as its LGPL lets them, names it in
	# NPT_D3D11_LIBRARY_PATH and runs a build of their own: gpu3d/SOURCES.)
	for f in "$APP/Contents/Resources/gpu3d"/*.dylib "$APP/Contents/Resources/gpu3d/virgl_render_server"; do
		codesign --force --options runtime --timestamp --sign "$ID" "$f"
	done
	codesign --force --options runtime --timestamp --sign "$ID" --identifier dev.keelos.hypermac.bhyve \
		--entitlements vm/bhyve/hvf/bhyve.entitlements "$APP/Contents/MacOS/bhyve-3d"
fi
codesign --force --options runtime --timestamp --sign "$ID" "$APP"
codesign --verify --strict --deep "$APP"

# Notarize the app on its own too and staple it, so a copy in /Applications passes Gatekeeper
# offline (the dmg's ticket covers it only while it is checked from the dmg or online).
notarize() { # file
	LOG=$OUT/notary-$(basename "$1").json
	xcrun notarytool submit "$1" --keychain-profile hypermac-notary --wait --output-format json > "$LOG" || true
	SUB=$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$LOG" | head -1)
	if ! grep -q '"status" *: *"Accepted"' "$LOG"; then
		cat "$LOG" >&2
		[ -n "$SUB" ] && xcrun notarytool log "$SUB" --keychain-profile hypermac-notary >&2
		die "notarization of $(basename "$1") failed"
	fi
	echo "notarized $(basename "$1") ($SUB)"
}
ZIP=$OUT/$NAME-app.zip
ditto -c -k --keepParent "$APP" "$ZIP"
notarize "$ZIP"
rm -f "$ZIP"
xcrun stapler staple "$APP"

# 5. dmg: the app, a link to /Applications (drag arrow between them), the notices, a short README --------------------
mkdir -p "$STAGE/Licenses"
for n in THIRD_PARTY.md LICENSE-IronRDP-MIT THIRD_PARTY-crates.txt; do cp "$n" "$STAGE/Licenses/"; done
mv "$STAGE/THIRD_PARTY-vm.txt" "$STAGE/Licenses/"
cat > "$STAGE/README.txt" <<TXT
hyperMAC $VER ($COMMIT) — RDP server for this Mac's desktop, and virtual machines on it
                        把这台 Mac 的桌面开成 RDP 服务，并在它上面跑虚拟机

Install:
  1. Drag hyperMAC into Applications.
  2. Open hyperMAC from Applications: type your Mac's login password once (clients log in with your
     Mac account), allow Screen Recording and Accessibility (System Settings > Privacy & Security),
     then it starts and runs at login.
  3. Connect with kRDPclient (recommended: H.264, sound, clipboard):
     https://github.com/keelos-dev/kRDPclient-releases
     mstsc and other RDP clients work too, but without H.264 the picture is less smooth.
Virtual machines (new, a preview): the window's "Virtual Machines" tab makes macOS, Windows 11
  (ARM) and Linux / FreeBSD machines; kRDPclient lists them after this Mac. A Windows machine has
  a TPM 2.0 and Secure Boot: Microsoft's installer runs as it is.
Update: hyperMAC menu > "Check for Updates…". From a dmg: open the hyperMAC IN the dmg (double-click
  it there) and click Update; dragging it over the installed one fails while hyperMAC runs.
Uninstall: open hyperMAC > "Uninstall…", then delete hyperMAC.app and
  ~/Library/Application Support/hyperMAC.

安装：
  1. 把 hyperMAC 拖进「应用程序」。
  2. 从「应用程序」打开 hyperMAC：输一次这台 Mac 的登录密码（客户端就用你的 Mac 账号登录），
     在「系统设置 → 隐私与安全性」里允许「屏幕录制」和「辅助功能」，之后服务启动并随登录自启。
  3. 推荐用 kRDPclient 连（H.264、声音、剪贴板）：https://github.com/keelos-dev/kRDPclient-releases
     mstsc 等标准 RDP 客户端也能连，但没有 H.264，画面流畅度差一截。
虚拟机（新功能，预览）：窗口的「虚拟机」页可以建 macOS、Windows 11（ARM）、Linux / FreeBSD 虚拟机，
  kRDPclient 里它们列在这台 Mac 后面。Windows 虚拟机带 TPM 2.0 和 Secure Boot，微软的安装程序原样运行。
更新：hyperMAC 菜单 →「检查更新…」。用 dmg 更新：直接双击 dmg 里的 hyperMAC，点「更新」；
  hyperMAC 在运行时拖过去覆盖会被系统拒绝。
卸载：打开 hyperMAC →「卸载…」，再删掉 hyperMAC.app 和 ~/Library/Application Support/hyperMAC。

Third-party notices / 第三方声明: the Licenses folder / Licenses 文件夹
More / 详情: https://github.com/keelos-dev/hyperMAC
TXT
DMG=$OUT/$NAME.dmg
rm -f "$DMG"
# The window layout (icons, arrow background) comes from dmgbuild (tools/dmg-settings.py), kept
# in a venv of its own under ~/Library/Caches (Python 3.11+).
VENV=$HOME/Library/Caches/hypermac-dmgbuild
if [ ! -x "$VENV/bin/dmgbuild" ]; then
	python3.11 -m venv "$VENV" && "$VENV/bin/pip" install -q dmgbuild==1.6.7
fi
"$VENV/bin/dmgbuild" -s tools/dmg-settings.py -D stage="$STAGE" -D background="$D/assets/dmg-background.tiff" hyperMAC "$DMG"
codesign --force --timestamp --sign "$ID" "$DMG"

# 6. notarize + staple the dmg ----------------------------------------------------------------
notarize "$DMG"
xcrun stapler staple "$DMG"

# 7. Gatekeeper's verdict on both ------------------------------------------------------------
spctl -a -vv -t open --context context:primary-signature "$DMG" 2>&1 | tee "$OUT/spctl.txt"
spctl -a -vv "$APP" 2>&1 | tee -a "$OUT/spctl.txt"
grep -c "accepted" "$OUT/spctl.txt" | grep -qx 2 || die "spctl did not accept both"
grep -c "Notarized Developer ID" "$OUT/spctl.txt" | grep -qx 2 || die "spctl: not Notarized Developer ID"
rm -rf "$STAGE"

# 8. changelog.json beside the dmg (attach both to the release): the newest 400 commits without
# the documentation-only ones, as kRDPclient's package.ps1 writes it; the app's update dialog
# lists those between the running build and the new one.
git log -400 --format='%h%x09%cI%x09%s' | python3 -c '
import json, re, sys
out = []
for line in sys.stdin:
    h, t, s = line.rstrip("\n").split("\t", 2)
    if re.match(r"^(docs|README|TODO-release|CLAUDE\.md)", s):
        continue
    out.append({"hash": h, "time": t, "subject": s})
json.dump({"format": 1, "commits": out}, sys.stdout, ensure_ascii=False)
' > "$OUT/changelog.json"

# 9. -----------------------------------------------------------------------------------------
echo
echo "$DMG"
echo "$OUT/changelog.json"
echo "sha256 $(shasum -a 256 "$DMG" | cut -d' ' -f1)"
[ -z "$DIRTY" ] || echo "WARNING: $DIRTY — do not publish this dmg"
