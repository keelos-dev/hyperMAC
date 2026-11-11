#!/bin/sh
# Put IronRDP into vendor/IronRDP: keel-rdp's pin with ironrdp-keel.patch applied (CredSSP
# takes the password or its NT hash by user name, Hybrid security also accepts TLS-only clients).
# Shared by build.sh (daily development) and tools/package.sh (release). Re-made only when the
# pin or a patch changed.
set -e
D=$(cd "$(dirname "$0")/.." && pwd)
REV=8f9bf5b8102dd3c4cdb4841939d4dbba35c7bff8
V=$D/vendor/IronRDP
SUM=$(cat "$D/ironrdp-keel.patch" "$D/ironrdp-hypermac.patch" | shasum -a 256 | cut -d' ' -f1)
if [ "$(cat "$V/.keel-rev" 2>/dev/null)" != "$REV $SUM" ]; then
	rm -rf "$V" && mkdir -p "$D/vendor"
	git clone -q https://github.com/Devolutions/IronRDP.git "$V"
	git -C "$V" checkout -q "$REV"
	git -C "$V" apply "$D/ironrdp-keel.patch"
	git -C "$V" apply "$D/ironrdp-hypermac.patch"	# hyperMAC's own, on top (its head says what)
	echo "$REV $SUM" > "$V/.keel-rev"
fi
