#!/bin/sh
# gpu3d.sh: the 3D engine for Windows guests (vm.json "gpu3d": true, vm/src/bhyve.rs gpu3d()).
#
#	build/bhyve-3d		bhyve with the 3D half of the virtio-gpu (hvf/pci_virtio_gpu_3d.c)
#	build/gpu3d/		what it needs beside it, nothing outside this folder:
#	  libvirglrenderer.1.dylib	the renderer's library in bhyve (UTM's virglrenderer, MIT)
#	  libepoxy.0.dylib		which that one links (MIT)
#	  virgl_render_server		one process per guest process that draws
#	  libdxmt-native.dylib		Direct3D 11 on Metal, loaded by the server (DXMT, LGPL 2.1)
#	  driver/			the guest's driver (Triton's viogpu3d, signed by Microsoft)
#	  LICENSES/, SOURCES		the licenses, and where each part's source is
#
# Needs (ask before installing): brew install meson ninja llvm@15 libepoxy, Xcode with the Metal
# toolchain (xcodebuild -downloadComponent MetalToolchain), python3 with PyYAML for meson (a
# virtualenv is made in build/ when it has none). The first run builds DXMT and virglrenderer
# from source: about ten minutes.
#
#	GPU3D_PREFIX=DIR	an install of both that is there already (meson --prefix DIR): no build
#	GPU3D_SRC=DIR		their source trees (DIR/dxmt, DIR/virglrenderer; the licenses are
#				copied from there), default build/gpu3d-src
#	GPU3D_DRIVER=no		do not download the guest's driver
set -e
D=$(cd "$(dirname "$0")" && pwd)
cd "$D"

# what is built: UTM's forks, at the commits this was tested with (2026-11-11)
VIRGL_URL=https://github.com/utmapp/virglrenderer.git
VIRGL_REV=dd0652b13e589c4c124ea79123a3adcd85173f4f	# branch macos-next
DXMT_URL=https://github.com/utmapp/dxmt.git
DXMT_REV=1fe2e752c98207518a99c9322482f63ebb9f0232
DRIVER_URL=https://github.com/osy/kvm-guest-drivers-windows/releases/download/v0.5/viogpu3d-arm64x-signed.zip
DRIVER_SRC="https://github.com/osy/kvm-guest-drivers-windows (branch neptune), https://github.com/osy/virtio-win-mesa (branch neptune)"
LLVM=${LLVM15:-/opt/homebrew/opt/llvm@15}

S=${GPU3D_SRC:-build/gpu3d-src}
P=${GPU3D_PREFIX:-$D/build/gpu3d-prefix}
if [ -z "$GPU3D_PREFIX" ]; then
	for t in meson ninja git; do
		command -v $t > /dev/null || { echo "gpu3d.sh: $t is missing (brew install meson ninja llvm@15 libepoxy)" >&2; exit 1; }
	done
	[ -x "$LLVM/bin/llvm-config" ] || { echo "gpu3d.sh: LLVM 15 is missing ($LLVM): brew install llvm@15" >&2; exit 1; }
	xcrun -sdk macosx metal -v > /dev/null 2>&1 ||
	    { echo "gpu3d.sh: the Metal toolchain is missing: xcodebuild -downloadComponent MetalToolchain" >&2; exit 1; }
	if ! python3 -c 'import yaml' 2> /dev/null; then
		[ -x build/gpu3d-venv/bin/python3 ] || { python3 -m venv build/gpu3d-venv && build/gpu3d-venv/bin/pip -q install pyyaml; }
		PATH="$D/build/gpu3d-venv/bin:$PATH"
	fi
	get() { # url, rev, dir
		[ -d "$S/$3/.git" ] || git clone -q "$1" "$S/$3"
		git -C "$S/$3" fetch -q origin "$2" 2> /dev/null || true
		git -C "$S/$3" checkout -q "$2"
	}
	mkdir -p "$S"
	get "$DXMT_URL" "$DXMT_REV" dxmt
	git -C "$S/dxmt" submodule update -q --init --depth 1 include/native/directx
	get "$VIRGL_URL" "$VIRGL_REV" virglrenderer
	echo "gpu3d.sh: building DXMT (build/gpu3d-dxmt.log)"
	(cd "$S/dxmt" && { [ -d build ] || meson setup build -Dnative_llvm_path="$LLVM" --buildtype release --prefix "$P"; } &&
	    meson compile -C build && meson install -C build) > build/gpu3d-dxmt.log 2>&1 ||
	    { echo "gpu3d.sh: DXMT did not build" >&2; exit 1; }
	echo "gpu3d.sh: building virglrenderer (build/gpu3d-virgl.log)"
	# Neptune (Direct3D) only, in render server processes: no Venus (Vulkan), no vtest
	(cd "$S/virglrenderer" && { [ -d build ] || meson setup build --prefix "$P" --buildtype release -Dneptune=true \
	    -Dvenus=false -Dvtest=false -Dtests=false -Drender-server-worker=process; } &&
	    meson compile -C build && meson install -C build) > build/gpu3d-virgl.log 2>&1 ||
	    { echo "gpu3d.sh: virglrenderer did not build" >&2; exit 1; }
fi
for f in lib/libvirglrenderer.1.dylib lib/libdxmt-native.dylib libexec/virgl_render_server include/virgl/virglrenderer.h; do
	[ -e "$P/$f" ] || { echo "gpu3d.sh: $P/$f is missing" >&2; exit 1; }
done

# build/gpu3d: copies, so that their names can be made relative (the installed ones stay as they are)
G=build/gpu3d
rm -rf "$G.new"
mkdir -p "$G.new/LICENSES"
cp "$P/lib/libvirglrenderer.1.dylib" "$P/lib/libdxmt-native.dylib" "$P/libexec/virgl_render_server" "$G.new/"
EPOXY=$(otool -L "$P/lib/libvirglrenderer.1.dylib" | awk '/libepoxy/ { print $1 }')
[ -n "$EPOXY" ] && [ -e "$EPOXY" ] || { echo "gpu3d.sh: libvirglrenderer's libepoxy is not there ($EPOXY)" >&2; exit 1; }
cp "$EPOXY" "$G.new/libepoxy.0.dylib"
chmod u+w "$G.new"/*.dylib "$G.new/virgl_render_server"
install_name_tool -id @rpath/libvirglrenderer.1.dylib -change "$EPOXY" @loader_path/libepoxy.0.dylib "$G.new/libvirglrenderer.1.dylib"
install_name_tool -id @rpath/libepoxy.0.dylib "$G.new/libepoxy.0.dylib"
install_name_tool -id @rpath/libdxmt-native.dylib "$G.new/libdxmt-native.dylib"
for f in "$G.new"/*.dylib "$G.new/virgl_render_server"; do
	# anything else from outside the system would be missing on another Mac
	if otool -L "$f" | tail -n +2 | grep -v -E '^\s*(/usr/lib/|/System/|@rpath/|@loader_path/)' | grep -q .; then
		echo "gpu3d.sh: $f still needs:" >&2
		otool -L "$f" | tail -n +2 | grep -v -E '^\s*(/usr/lib/|/System/|@rpath/|@loader_path/)' >&2
		exit 1
	fi
	codesign --force -s - "$f" 2> /dev/null
done 2>&1 | grep -v 'will invalidate the code signature' || true

# the guest's driver: the release's files as they are (their catalog is signed by Microsoft)
if [ "${GPU3D_DRIVER:-yes}" != no ]; then
	[ -f build/gpu3d-driver.zip ] || curl -sfL -o build/gpu3d-driver.zip "$DRIVER_URL" ||
	    { rm -f build/gpu3d-driver.zip; echo "gpu3d.sh: the guest's driver did not download ($DRIVER_URL)" >&2; exit 1; }
	rm -rf build/gpu3d-driver && mkdir -p build/gpu3d-driver
	unzip -q -o build/gpu3d-driver.zip -d build/gpu3d-driver
	inf=$(find build/gpu3d-driver -name viogpu3d.inf | head -1)
	[ -n "$inf" ] || { echo "gpu3d.sh: no viogpu3d.inf in the driver's archive" >&2; exit 1; }
	mkdir -p "$G.new/driver"
	cp "$(dirname "$inf")"/* "$G.new/driver/"
fi

lic() { # file, name: the source tree's when it was built here, else say where it is
	if [ -f "$1" ]; then cp "$1" "$G.new/LICENSES/$2"; fi
}
lic "$S/virglrenderer/COPYING" virglrenderer-MIT.txt
lic "$S/dxmt/LICENSE" dxmt-LICENSE.txt
lic "$S/dxmt/COPYING.LIB" dxmt-LGPL-2.1.txt
lic "$(dirname "$(dirname "$EPOXY")")/COPYING" libepoxy-MIT.txt
cat > "$G.new/SOURCES" << EOF
What is in this folder and where its source is.

virgl_render_server, libvirglrenderer.1.dylib
	virglrenderer with Neptune (Direct3D over virtio-gpu), MIT
	$VIRGL_URL  commit $VIRGL_REV

libdxmt-native.dylib
	DXMT, Direct3D 11 on Metal, GNU Lesser General Public License 2.1 (LICENSES/)
	$DXMT_URL  commit $DXMT_REV
	Built with vm/bhyve/gpu3d.sh of hyperMAC's source. To use another build of it: replace
	the file (the render server loads it by this name), or set NPT_D3D11_LIBRARY_PATH and
	NPT_DXGI_LIBRARY_PATH to yours.

libepoxy.0.dylib
	libepoxy, MIT: https://github.com/anholt/libepoxy

driver/
	Triton, the Windows guest's driver (viogpu3d), as released (signed by Microsoft):
	$DRIVER_URL
	source: $DRIVER_SRC
EOF
rm -rf "$G.old"
if [ -e "$G" ] || [ -L "$G" ]; then mv "$G" "$G.old"; fi
mv "$G.new" "$G"
rm -rf "$G.old"

# bhyve-3d, linked with the copy: it finds the library in gpu3d/ beside itself
[ -d build/src ] || ./build.sh > build.log 2>&1
rm -f build/bhyve-3d	# linked again whatever make thinks (a VM that runs from it keeps its file)
make -j"$(sysctl -n hw.ncpu)" VIRGL="$P" GPU3D_LIB="$D/$G/libvirglrenderer.1.dylib" > build/gpu3d-bhyve.log 2>&1 ||
    { echo "gpu3d.sh: bhyve-3d did not build (build/gpu3d-bhyve.log)" >&2; exit 1; }
echo "gpu3d.sh: build/bhyve-3d, build/gpu3d ($(du -sh "$G" | cut -f1))"
