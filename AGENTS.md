# For AI coding agents (and people in a hurry)

You are in hyperMAC's source: an RDP server for a Mac's own desktop plus a hypervisor front end
(macOS / Linux guests on Apple's Virtualization.framework, Windows 11 ARM / FreeBSD guests on a
port of FreeBSD's bhyve to Hypervisor.framework). This file tells you how to get from a fresh
clone to a running build, and what not to break. `README.md` is for users; `docs/DEVELOPING.md`
(Chinese) has the design notes.

## What is needed

An Apple Silicon Mac, macOS 26 or later. Intel Macs and other systems cannot build or run it.

Check, and install what is missing (ask the person before installing anything):

```sh
xcode-select -p            || xcode-select --install          # compiler, codesign, the frameworks' headers
command -v cargo           || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustup target add aarch64-unknown-uefi                        # one small EFI program (vm/tools/efiprobe)
command -v brew            || echo "Homebrew: see https://brew.sh"
brew install cmake libtpms openssl                            # cmake: libopus builds from source; libtpms + openssl: the TPM inside bhyve
python3 --version                                             # 3.9 or later (the bhyve build runs generators); Xcode's is fine
```

Without `libtpms` and `openssl` everything still builds; Windows guests then have no TPM and
their installer runs with its hardware checks switched off.

## Build

```sh
sh build.sh
```

About five minutes the first time. It downloads IronRDP's sources once (`vendor/IronRDP`, pinned),
and makes:

| File | What |
|---|---|
| `target/release/hypermac` | the main program: RDP service, window, menu bar icon |
| `vm/target/release/hypermac` | the VM program (one process per running VM) |
| `vm/bhyve/build/bhyve` | the bhyve engine (its own log: `vm/bhyve/build.log`) |

A good build ends with exit status 0 and all three files there. Check the TPM made it in:
`strings vm/bhyve/build/bhyve | grep -c "libtpms needs"` prints 1.

`build.sh` signs with the first "Apple Development" identity in the keychain when there is one,
else ad hoc (works; macOS then asks for the two privacy permissions again after each rebuild).

## Try it without installing anything

The VM program runs straight from the tree. Use a scratch state folder with a SHORT path (a
running VM keeps Unix sockets in it; their path is limited to 103 bytes):

```sh
export HYPERMAC_STATE=/tmp/hm-try
vm/target/release/hypermac list
vm/target/release/hypermac create f1 --os freebsd --iso /path/to/FreeBSD-15.1-RELEASE-arm64-aarch64-disc1.iso
vm/target/release/hypermac run f1 --console        # a window with the guest's screen
vm/target/release/hypermac ctl f1 stop             # ACPI power button
```

Windows 11 ARM: `create w1 --os windows --iso Win11_ARM64.iso --virtio virtio-win.iso --manual`
(or without `--manual` and with `HYPERMAC_WIN_PASSWORD=...` for an unattended install).
macOS: `create m1 --ipsw latest` (downloads about 25 GB from Apple).

## Install the app from source

```sh
target/release/hypermac install     # two LaunchAgents: the service and the menu bar icon
sh build.sh                         # again: now it also puts the VM program and bhyve beside the installed copy
# then click the menu bar icon: the window's first-run pages ask for the Mac's login password and the two permissions
```

This changes the person's machine (a login item, an RDP port on 3389, an HTTPS port on 443): do
it only when they ask for an install, not as part of "build it". Most people should install the
signed release from GitHub instead. `target/release/hypermac uninstall` removes the login items.

## Rules of the tree

- `vm/bhyve/freebsd/` is FreeBSD's source, unchanged. Never edit it. Changes are numbered patches
  in `vm/bhyve/patches/` (`upstream/`: could go to FreeBSD; `macos/`: only for this port), made
  with `vm/bhyve/mkpatch.sh`. Edit in `vm/bhyve/build/src`, then run mkpatch. `vm/bhyve/README.md`
  explains the layout.
- `vm/bhyve/keel/` are generators and files from keelOS: they patch FreeBSD's text at anchors. If
  a patch you add moves an anchor, `build.sh` says which generator no longer fits.
- `vendor/` and `target/` are build output; do not commit them.
- Sending `SIGTERM` to a bhyve process is the guest's power button, not a kill. To end one for
  sure: `ctl NAME kill`.
- Do not restart or unload `dev.keelos.hypermac` (the service) without asking: it may be how the
  person is connected to this Mac right now.
- Tests: `cargo test --release` in the root. There is no automated test of the virtual machines;
  say plainly what you ran and what you only compiled.
- Releases are made by the maintainer (`tools/package.sh` needs a Developer ID certificate and a
  notarization profile). This repository receives one commit per release from a private
  development repository; send changes as pull requests.

## When something fails

- `error[E0463]: can't find crate for core` in efiprobe: the `aarch64-unknown-uefi` target is missing.
- `vm/bhyve did not build`: read `vm/bhyve/build.log`. "does not apply" = a patch no longer fits
  the FreeBSD sources; "does not fit" = a keelOS generator's anchor moved.
- A VM ends at once with `hv_vm_create` failing: the binary lost its entitlement (it was rebuilt
  by `cargo build` alone). Run `sh build.sh`, which signs it again.
- The window says the VM program is missing: `sh build.sh` once more after `install`.
