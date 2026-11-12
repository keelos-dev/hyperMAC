<p align="center"><img src="docs/img/logo.png" width="140" alt="hyperMAC"></p>
<h1 align="center">hyperMAC</h1>
<p align="center"><b>Windows 11 on your Mac in three clicks. Free, open source, no QEMU.</b><br>
bhyve on Apple Silicon + Apple's Virtualization.framework + a built-in RDP server, in one app.</p>
<p align="center"><a href="https://github.com/keelos-dev/hyperMAC/releases">Download</a> · <a href="#windows-11-in-three-steps">Windows 11 in three steps</a> · <a href="#how-it-compares">How it compares</a> · <a href="README.zh-CN.md">中文</a></p>

![Windows 11 ARM running in hyperMAC on an M4 Mac](docs/img/windows11.jpg)

hyperMAC does two things, and they fit together:

1. **A hypervisor.** macOS, Windows 11 ARM, Linux and FreeBSD guests on Apple Silicon. Windows and
   FreeBSD run on **bhyve**, FreeBSD's hypervisor, ported to Apple's Hypervisor.framework: as far
   as we know the first bhyve on Apple Silicon, and the first bhyve anywhere to give a Windows
   guest 3D acceleration without handing it a real graphics card (Direct3D 11, drawn by the
   Mac's GPU). macOS and Linux guests run on Apple's own Virtualization.framework.
2. **An RDP server.** Your Mac's own desktop and its virtual machines are reachable with plain RDP:
   Windows' built-in `mstsc` connects with nothing to install. A Mac mini in a closet becomes a
   Windows-and-Mac server you use from any PC.

Don't want the second part? One switch closes every port, and it is just a local hypervisor.

| New Virtual Machine: pick a system | The Mac itself: remote access, one switch each |
|---|---|
| ![The Virtual Machines tab and the New Virtual Machine window](docs/img/vm-wizard.png) | ![The Host tab: remote access settings](docs/img/host-settings.png) |

## Why you might want it

- **Windows 11 ARM installs from Microsoft's own ISO, unmodified.** The machine has a TPM 2.0 and
  Secure Boot, so the installer's hardware checks pass as they are: no registry tricks, no patched
  image. Choose "unattended" and you are at the desktop in about 8 minutes on an M4, account made,
  network up.
- **Free and open source.** MIT; the bhyve port BSD-2-Clause. No subscription, no account, no
  telemetry.
- **Small and native.** One app of about 30 MB: Rust and C, no QEMU, no kernel extension, no
  background daemon running as root. A virtual machine is one ordinary process.
- **Headless by design.** Virtual machines open no window on the Mac and keep running when the
  app's window closes. Look at them in a console window, or from another computer over RDP.
- **Windows programs among your Mac's windows.** Coherence (new, experimental): each window of
  a Windows guest becomes a window on the Mac's desktop, with no Windows desktop around it.
- **Snapshots and clones that cost nothing.** A stopped machine's snapshot or clone is made
  at once and takes no room of its own (APFS clones: only what changes afterwards is stored).
- **Two engines, each where it is best.** Apple's Virtualization.framework for macOS and Linux
  (it is what Apple supports). bhyve for Windows and FreeBSD, where Virtualization.framework
  does not go.

## Windows 11 in three steps

1. Open hyperMAC → **Virtual Machines** → **New Virtual Machine…** → **Windows 11**.
2. Get the **Windows 11 ARM64 ISO**: the window has a button that opens
   [Microsoft's download page](https://www.microsoft.com/software-download/windows11arm64). Choose
   a language there and download; when the file is in Downloads the window shows it by itself.
   (The driver ISO the install needs, virtio-win, is downloaded for you.)
3. Click **Create**, then **Start**. Install as on a real PC in the console window; or switch on
   "unattended", type a user name and password, and come back in 8 minutes.

That is all. No TPM bypass, no "this PC can't run Windows 11".

| The firmware shows where you are | Windows agrees it is a real, secure PC |
|---|---|
| ![UEFI boot screen with the hyperMAC logo](docs/img/firmware-logo.png) | ![Windows System Information: Secure Boot on](docs/img/secure-boot.jpg) |

macOS guests are as short: **New Virtual Machine… → macOS → Download the newest macOS from
Apple**, and it installs by itself. Linux and FreeBSD: point it at the distribution's arm64 ISO.

## What runs

| Guest | Engine | How it installs | State |
|---|---|---|---|
| **Windows 11 ARM** | bhyve on Hypervisor.framework | Microsoft's ISO: by hand, or unattended | TPM 2.0, Secure Boot, network, 1920x1080, Windows Update, sound, 3D acceleration (Direct3D 11, opt-in) |
| **macOS** | Virtualization.framework | by itself, from Apple's restore image (downloaded for you) | graphics acceleration (Apple's paravirtual GPU), smooth |
| **Linux** (arm64) | Virtualization.framework | the distribution's ISO | Fedora tested |
| **FreeBSD** (arm64) | bhyve on Hypervisor.framework | the release's ISO | 15.1 tested |

## How it compares

From each project's public documentation, 2026-11; not all tested side by side. Corrections are
welcome as an issue.

| | **hyperMAC** | UTM | Parallels Desktop | VMware Fusion | VirtualBox |
|---|---|---|---|---|---|
| Price | free | free | paid subscription | free | free |
| Open source | ✅ MIT / BSD | ✅ | ❌ | ❌ | ✅ (GPL) |
| What runs the guest | bhyve on Hypervisor.framework; Apple VZ | QEMU; Apple VZ | its own | its own | its own |
| Windows 11 ARM | ✅ | ✅ | ✅ | ✅ | ✅ (7.2 and later) |
| macOS guests | ✅ | ✅ | ✅ | ❌ | ❌ |
| 3D acceleration in Windows | ✅ Direct3D 11 (new, opt-in) | ✅ Direct3D 11, and 12 where Apple's D3DMetal is there (5.0.8) | ✅ DirectX 11.1, OpenGL 4.3 | ✅ DirectX 11 | experimental (7.2) |
| Games in Windows | Direct3D 11 titles; numbers below | Direct3D 11 and 12 titles | Direct3D 11 titles; the longest record with games | Direct3D 11 titles | not something to count on yet |
| The game's picture and sound on another PC | ✅ RDP, H.264 at 60 fps, sound | ❌ | ❌ | ❌ | ❌ |
| RDP to the Mac's own desktop and to the VMs | ✅ built in (a macOS guest runs hyperMAC itself) | ❌ | ❌ | ❌ | VMs only (VRDE) |

Games: Direct3D 11 titles run, with sound. Measured with Unigine Heaven 4.0 (Direct3D 11; an
x64 program, so Windows translates it) on a Mac mini M4 with 16 GB, the guest with 4 cores and
8 GB, 2026-11-11, the whole benchmark run:

| Heaven 4.0 | Score | Average | Lowest | Highest |
|---|---|---|---|---|
| 1920x1080 full screen, 2x anti-aliasing, quality High, tessellation off | 1967 | 78.1 fps | 12.9 | 173.5 |

Seen from another PC in kRDPclient during that run: 1920x1080 H.264 at 64 fps, 11.6 Mbps, no
frame skipped, the guest's sound as Opus.

Honestly, though: this is new, and one benchmark and a browser are what has been tried. Every
program in the table can run Direct3D 11 games on Apple Silicon by now; UTM (whose author wrote
the guest driver used here) also does Direct3D 12, and Parallels has years of work on game
compatibility behind it. No Direct3D 12, OpenGL or Vulkan here, and games with kernel anti-cheat
do not start in a virtual machine. What only hyperMAC does is put that game on another PC's
screen, with its sound. If you
want Windows 11 for Office, a browser, development tools or that one Windows-only program, on a
Mac that may not even have a screen attached, and you would rather not pay every year for it:
this is for you.

## The RDP server

hyperMAC began as an RDP server for the Mac itself, and that part is the mature one.

- **`mstsc` just connects** (`mac-address:3389`, your Mac's user name and password).
- With **[kRDPclient](https://github.com/keelos-dev/kRDPclient-releases)** (Windows, free) you get
  hardware H.264: 1080p at 30 fps (60 on a LAN), about 6–10 Mbps, with sound and the clipboard. It
  lists the Mac and each virtual machine; one click opens any of them.
- Windows, Linux and FreeBSD guests are served by the host: nothing to install in them. A **macOS
  guest** is a Mac: install hyperMAC inside it too and it serves its own desktop (give it bridged
  networking so other computers reach it).
- The login is your Mac account. No cloud service sits in between.
- Free, also where NoMachine no longer is: its version 10 dropped the free server edition, and a
  server now needs a yearly subscription, for personal use too.

### Compared with other remote desktops for the Mac

From each product's public description (2026-11); not all of them tested side by side.

| | hyperMAC | Apple Screen Sharing | NoMachine | Chrome Remote Desktop | RustDesk | Parsec |
|---|---|---|---|---|---|---|
| Protocol | RDP | VNC + Apple extensions | NX | WebRTC | own | own |
| Video codec | H.264 hardware (kRDPclient); RemoteFX / bitmaps (mstsc); sound Opus / PCM | VNC encodings (Raw/ZRLE/Tight…); HEVC in high-performance mode (Mac to Mac, Apple Silicon) | H.264, VP8 | VP8 / VP9 (WebRTC) | VP8 / VP9 / AV1, optional hardware H.264 / H.265 | H.264, H.265 |
| Windows' built-in mstsc connects | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Account / cloud service needed | no | no | no (LAN) | Google account | no (self-hosted relay possible) | yes |
| Hardware video to a Windows client | ✅ H.264 (kRDPclient) | ❌ | ✅ | — | ✅ | ✅ |
| Smoothness (Windows client) | smooth: 1080p 30 fps (60 on a LAN), measured 7.6 ms decode, 6–10 Mbps, ~6 % Mac CPU | choppy (tested with TigerVNC) | smooth | fair | fairly smooth | very smooth (game-grade latency) |
| Sound | ✅ | high-performance mode only (Mac clients) | ✅ | ✅ | ✅ | ✅ |
| File transfer | ❌ (planned) | ✅ (Mac clients) | ✅ | ❌ | ✅ | ❌ |
| Works at the login window | ❌ | ✅ | ✅ | ✅ | ✅ | — |
| Price | free | built in | paid from version 10 on (also for personal use) | free | open source | free for personal use |

Pick hyperMAC when the other side is Windows and you want plain RDP (mstsc with nothing to install,
or kRDPclient for hardware H.264), with no cloud account in between.

### Features by macOS version

✅ tested on a real Mac · ☑️ should work, not yet tested on this version · — not tested

| Feature | 13–15 | 26 | 27.2 |
|---|:-:|:-:|:-:|
| kRDPclient: login, H.264 hardware video | — | ✅ | ✅ |
| mstsc / standard RDP clients (RemoteFX) | — | ✅ | ☑️ |
| Keyboard and mouse (input methods work) | — | ✅ | ✅ |
| Sound (Opus to kRDPclient, PCM to mstsc) | — | ✅ | ☑️ |
| Clipboard: text | — | ✅ | ☑️ |
| Clipboard: pictures | — | ✅ | ✅ |
| Found by kRDPclient's "Discover keelOS" | — | ✅ | ☑️ |
| Runs in the background, starts at login | — | ✅ | ✅ |
| Permissions kept across a macOS upgrade | — | — | ✅ (26 → 27.2) |
| App, first-run setup, Check for Updates | — | ✅ | ☑️ |

## Install

1. Download the newest `.dmg` from [Releases](https://github.com/keelos-dev/hyperMAC/releases) and
   open it. It is signed and notarized.
2. Drag **hyperMAC** into **Applications** and open it.
3. First run: type your Mac's login password once (that is what remote clients log in with) and
   allow **Screen Recording** and **Accessibility**. Only want virtual machines? Click
   **Virtual Machines Only** on that page and skip both.

Requirements: a Mac with Apple Silicon. Tested on macOS 26 and 27. Intel Macs are not supported.

## Which RDP client

**kRDPclient is the recommended client**: https://github.com/keelos-dev/kRDPclient-releases
(Windows). In kRDPclient choose "Add keelOS host", enter the Mac's address and your Mac's
login password (kRDPclient trades it for a token and does not keep the password), or a token made
in the hyperMAC app. It gets the picture as hardware-encoded H.264, plus sound and the clipboard.

mstsc (Remote Desktop Connection) and other standard RDP clients can connect too
(`mac-address:3389`, your Mac user name and login password), **but they have no H.264 channel**:
the picture goes as RemoteFX / bitmaps and is noticeably less smooth. If mstsc feels slow, try
kRDPclient before judging hyperMAC.

### Limits of the RDP server

- A user must be logged in on the Mac: at the login window (after a restart without automatic
  login) nobody can connect. Turn on automatic login in System Settings → Users & Groups.
- The Mac needs a display, or a dummy plug (HDMI headless adapter); with nothing attached macOS
  has no screen to capture.
- Clipboard: text and pictures (screenshots, copied images), no files yet.
- The mouse pointer is part of the picture; there is no client-side pointer yet.
- The login is your Mac account, so anyone who knows your Mac password can connect: use a good
  one, and keep port 3389/443 inside your network. hyperMAC does not store the password; for mstsc
  it keeps the password's NT hash in a file only your account can read. After changing the Mac
  password, the old one stops working at once; mstsc needs one login with the new one from
  kRDPclient (or open hyperMAC and type it), kRDPclient works right away.
- Updates are not checked by themselves: use "Check for Updates…" in the app.

## What is rough

- **This is young.** The virtual machine part is brand new. Keep copies of anything important.
- 3D acceleration in Windows is new and off unless asked for: a switch in New Virtual Machine,
  and in Edit… for a machine there is (that one hands Windows a CD with the driver: run its
  install.cmd once). It is Direct3D 11: the desktop, Edge and WebGL
  use it, and so do Direct3D 11 games (Unigine Heaven 4.0, 1920x1080 full screen, 2x
  anti-aliasing: 78 fps on average on a Mac mini M4; WebGL Aquarium, 500 fish: 57 fps at 1024x768 on
  a MacBook Air M4). Anti-aliasing goes up to 4x (what the Mac's GPU has): a game asking for 8x
  says it cannot set the video mode. No Direct3D 12, OpenGL or Vulkan. Without it Windows has
  the basic display driver, up to 1920x1200.
- Coherence (the switch on a running Windows machine's row; it needs 3D acceleration to give
  Windows a screen the size of the Mac's) is a first version. While it is on, a button in the
  Mac's menu bar opens the Start menu, the items on Windows' desktop and File Explorer. The
  pointer keeps the Mac's arrow whatever Windows would show; one Windows pixel is one point,
  so it is soft on a Retina display; a Mac window between two overlapping Windows windows
  makes the lower one show a piece of the upper one; menus have a thin edge of what is behind
  them in Windows; the clipboard is not shared; only the main display. The left Command key
  is Windows' Control there (Cmd-C copies), the right one the Windows key.
  The Mac talks to a small program in Windows through a port of the machine's own, not over
  the network. A machine made with this version has everything for it; one made earlier gets
  a CD when the switch is first used (run its install.cmd in Windows as administrator, once)
  and must be restarted once. Until then it is reached over the network, which needs NAT
  networking and macOS' permission for the local network.
  Used by hand for an afternoon on one machine (windows, typing, the Start menu, a Direct3D
  benchmark in a window), not for days. Windows' snap layouts bar is off while it is on.
- Snapshots and clones are of a stopped machine (a running one's disk is in the middle of
  things). A clone has a new network address; the computer name and accounts inside are the
  original's.
- The screen follows the console window's size for Windows with 3D acceleration, for macOS
  guests, and for Linux on Virtualization.framework's own graphics. Linux as hyperMAC makes it
  (its own virtio-gpu, so RDP can show it), FreeBSD and Windows without 3D keep the size from
  Edit…, scaled into the window.
- Rosetta for Linux guests (x86-64 programs; a switch in New Virtual Machine and Edit…) needs
  one command run in the guest, which Edit… copies for you. Tried with Fedora 45; that the
  setup survives a restart of the guest is not tested yet.
- Start at Login starts a machine when you log in to the Mac, not when the Mac starts with
  nobody logged in.
- Sound from virtual machines: Windows only. It plays on the Mac while the VM's window is open,
  and goes to kRDPclient when that is connected to the VM (new). macOS, Linux and
  FreeBSD guests have no sound yet.
- Bridged networking, sleep/wake and long uptimes are not tested enough.
- The firmware's Secure Boot keeps its keys where the guest's kernel could reach them (the EDK2
  platform has no protected variable store): enough for Windows' requirements, not the strength
  of a real PC's.

## For the curious: bhyve on Apple Silicon

[xhyve](https://github.com/machyve/xhyve) brought bhyve to Intel Macs in 2015. `vm/bhyve` here is
the arm64 story:

- FreeBSD's `usr.sbin/bhyve` sources **unchanged**, with a numbered patch series on top and a small
  compatibility layer, so following FreeBSD head stays cheap.
- `libvmmapi` reimplemented over **Hypervisor.framework** (`vm/bhyve/hvf`, about 2,000 lines):
  vCPUs, the framework's in-kernel GICv3, MSIs through a GICv2m frame, instruction decoding for
  the MMIO accesses the CPU does not describe.
- What arm64 bhyve did not have and Windows needs: **UEFI** (EDK2), **ACPI** tables, a CFI flash
  for UEFI variables, QEMU's fw_cfg and ramfb, the **virtio 1.0** transport, a **TPM** (libtpms
  inside the process), Secure Boot.
- **3D for Windows guests**, which bhyve never had short of passing a whole graphics card
  through: a virtio-gpu 3D device (`vm/bhyve/hvf/pci_virtio_gpu_3d.c`) that carries the guest's
  Direct3D 11 calls to the Mac's GPU. It stands on other people's work: the guest's driver is
  Triton and the host's side is virglrenderer's Neptune, both from UTM's author, and DXMT turns
  Direct3D 11 into Metal. As far as we know this is the first time they run under bhyve.
- Eighteen of the patches are written for FreeBSD review, bugs found on the way included
  (`vm/bhyve/patches/REVIEW.md`). Nothing has been submitted yet.

## Update

Open hyperMAC and click "Check for Updates…" (under About). If there is a newer version it shows
what changed; "Update" downloads it, checks that it is signed by hyperMAC's developer and notarized
by Apple, replaces the app and restarts the service (a session in progress drops for a few
seconds). The permissions carry over.

From a downloaded dmg: open it and double-click the hyperMAC **inside the dmg**. It sees the
installed copy and offers **Update**: the same checks and the same replacement as above. Dragging
the new one over the old one in Finder does not work while hyperMAC runs ("hyperMAC is in use":
its menu bar icon and any running virtual machine count). Virtual machines keep running through
an update either way.

### Uninstall

1. Open hyperMAC and choose "Uninstall the login item".
2. Delete hyperMAC from Applications and the folder `~/Library/Application Support/hyperMAC`.
3. Optionally remove hyperMAC from Screen Recording and Accessibility in System Settings.

Remote Login (SSH) is a macOS setting and is left as it is; turn it off in System Settings →
General → Sharing if you no longer need it.

## Building from source

Needs Xcode's command line tools, Rust (rustup, stable, plus `rustup target add aarch64-unknown-uefi`),
Python 3 and `brew install cmake`; for the TPM of Windows machines also `brew install libtpms openssl`
(without them bhyve builds without a TPM).

```sh
sh build.sh        # target/release/hypermac, vm/target/release/hypermac (the VM program), vm/bhyve/build/bhyve
```

Building with an AI assistant? Point it at [AGENTS.md](AGENTS.md): what to install, how to build,
how to try a virtual machine from the tree, and what not to touch.

`build.sh` signs with the first "Apple Development" identity in the keychain when there is one
(the two permissions then survive a rebuild), else ad hoc. `target/release/hypermac install` sets
it up to run at login. How the parts fit together, and the release packaging, are in
[docs/DEVELOPING.md](docs/DEVELOPING.md) (in Chinese); the virtual machines in
[vm/README.md](vm/README.md) and [vm/bhyve/README.md](vm/bhyve/README.md).

This repository receives a snapshot per release from the development repository, one commit
each. Pull requests are welcome: a merged one is carried into the development repository and
is part of the next snapshot.

### License

hyperMAC's own code: MIT ([LICENSE](LICENSE)). `vm/bhyve`, the port of FreeBSD's bhyve to
Hypervisor.framework: BSD-2-Clause ([vm/bhyve/LICENSE](vm/bhyve/LICENSE)); FreeBSD's files in
`vm/bhyve/freebsd` keep their own headers.

## Third-party software

hyperMAC is built on [IronRDP](https://github.com/Devolutions/IronRDP) (MIT / Apache-2.0) and
other open-source Rust crates; its virtual machines use [bhyve](https://bhyve.org) from FreeBSD
(BSD licenses) and the [EDK2](https://www.tianocore.org) UEFI firmware (BSD-2-Clause-Patent).
Their notices ship with every copy: `THIRD_PARTY.md`, `LICENSE-IronRDP-MIT`,
`THIRD_PARTY-crates.txt` and `THIRD_PARTY-vm.txt` are in the dmg's `Licenses` folder and in
`hyperMAC.app/Contents/Resources/`.
