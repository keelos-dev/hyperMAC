#!/bin/sh
# sh vm/build.sh -> vm/target/release/hypermac, the VM program (one `hypermac run NAME` process per
# VM). The repo's build.sh builds it too and installs it as <state>/bin/hypermac-vm, signed with
# the Apple Development identity; here it is ad-hoc. Either way with the virtualization entitlement:
# Virtualization.framework refuses to start a VM without it.
set -e
cd "$(dirname "$0")"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
cargo build --release
codesign --force -s - --entitlements vz.entitlements target/release/hypermac
